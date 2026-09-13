//! The daemon binary starts over an entry, serves a read, and stops.
//!
//! Drives the real `au` binary end to end: `daemon start` boots and serves,
//! a client reads over the per-entry hashed socket, and `daemon stop` tears it
//! down, the process exiting and the socket removed. A directory entry and a
//! `*.au-workspace.yaml` file entry are both exercised, under a fake `$HOME` so
//! the real `~/.arsumbris/` is never touched.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{fs, thread};

use au_engine::{socket_file_name, Client};
use serde_json::json;

/// A minimal repo, plus a fake `$HOME` the daemon derives its hashed socket
/// under. Both live in tempdirs, so the test never touches the real
/// `~/.arsumbris/au-engine/run/` and never reads the developer's registry.
struct Fixture {
    repo: tempfile::TempDir,
    home: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let repo = tempfile::Builder::new()
        .prefix("au-daemon-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = repo.path();
    // The entry must be a folder-repo. Name it after the tempdir basename
    // (`au-daemon-XXXX`, a valid repo name) so the folder matches, no drift.
    let name = root.file_name().unwrap().to_str().unwrap().to_string();
    fs::create_dir(root.join(".arsumbris")).unwrap();
    fs::write(root.join(".arsumbris/repo.yaml"), format!("name: {name}\n")).unwrap();
    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  link?: file*\n",
    )
    .unwrap();
    fs::write(root.join("a.md"), "---\ntype: note\nlink: \"[[b]]\"\n---\n").unwrap();
    fs::write(root.join("b.md"), "---\ntype: note\n---\n").unwrap();
    let home = tempfile::Builder::new()
        .prefix("au-home-")
        .tempdir_in("/tmp")
        .unwrap();
    Fixture { repo, home }
}

/// The socket path the daemon binds under a given fake `$HOME`, mirroring
/// `au_engine::socket_path` with the home overridden.
fn socket_under(home: &Path, entry: &Path) -> std::path::PathBuf {
    home.join(".arsumbris")
        .join("au-engine")
        .join("run")
        .join(socket_file_name(entry))
}

/// Poll the socket until the daemon reports ready, returning the probe.
///
/// The deadline is generous: the spawned `au` binary pays a one-time macOS
/// first-exec security scan (~15s) on a freshly-linked build, so a 10s deadline
/// can expire before the daemon's `main` even starts under a cold gate run. 30s
/// matches the client read timeout and absorbs that scan.
fn wait_ready(socket: &Path) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(mut c) = Client::connect(socket) {
            if let Ok(resp) = c.query(&json!({ "read": "lifecycle" })) {
                if resp["ready"] == true {
                    return resp;
                }
            }
        }
        assert!(Instant::now() < deadline, "daemon never became ready");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn daemon_starts_serves_a_read_and_stops() {
    let fx = fixture();
    let root = fs::canonicalize(fx.repo.path()).unwrap();
    let home = fs::canonicalize(fx.home.path()).unwrap();
    let socket = socket_under(&home, &root);

    let mut child = Command::new(env!("CARGO_BIN_EXE_au"))
        .args(["daemon", "start"])
        .arg(&root)
        .env("HOME", &home)
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn daemon");

    // It builds once at start, so it is Ready at version 1 without an edit.
    let resp = wait_ready(&socket);
    assert_eq!(resp["version"], 1);
    assert_eq!(resp["result"]["lifecycle"]["ref"], "ready");

    // A real read resolves over the socket: a.md claims `note`.
    let mut client = Client::connect(&socket).expect("connect");
    let resp = client
        .query(&json!({ "read": "instance", "path": "a.md" }))
        .expect("instance read");
    assert_eq!(resp["ready"], true);
    assert!(
        resp["version"].is_number(),
        "a ready response carries a numeric version, got {resp:?}"
    );
    assert_eq!(resp["result"]["instance"]["resolved"], true);
    assert_eq!(resp["result"]["instance"]["claim"][0], "note");

    // `daemon stop` tears it down over the socket.
    let stop = Command::new(env!("CARGO_BIN_EXE_au"))
        .args(["daemon", "stop"])
        .arg(&root)
        .env("HOME", &home)
        .stdout(Stdio::null())
        .status()
        .expect("run daemon stop");
    assert!(stop.success(), "stop exits cleanly");

    // The process exits and removes the socket.
    let status = child.wait().expect("daemon exits");
    assert!(status.success(), "daemon exits cleanly, got {status:?}");
    assert!(!socket.exists(), "socket removed on shutdown");
}

/// The shutdown read is a read response: `ready` and `version` stay coupled,
/// so an SDK typing `ready: true => version: number` holds for it too.
#[test]
fn shutdown_read_couples_ready_and_version() {
    let fx = fixture();
    let root = fs::canonicalize(fx.repo.path()).unwrap();
    let home = fs::canonicalize(fx.home.path()).unwrap();
    let socket = socket_under(&home, &root);

    let mut child = Command::new(env!("CARGO_BIN_EXE_au"))
        .args(["daemon", "start"])
        .arg(&root)
        .env("HOME", &home)
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn daemon");

    wait_ready(&socket);

    let mut client = Client::connect(&socket).expect("connect");
    let resp = client
        .query(&json!({ "read": "shutdown" }))
        .expect("shutdown read");
    assert_eq!(resp["ready"], true);
    assert!(
        resp["version"].is_number(),
        "a ready shutdown response carries a numeric version, got {resp:?}"
    );
    assert_eq!(resp["result"]["shutting_down"], true);

    let status = child.wait().expect("daemon exits");
    assert!(status.success(), "daemon exits cleanly, got {status:?}");
    assert!(!socket.exists(), "socket removed on shutdown");
}

/// `daemon start` refuses a non-repo entry (a bare directory with no
/// `.arsumbris/repo.yaml`) with the NOT_A_REPO exit code, before binding a
/// socket. The `*.au-workspace.yaml` FILE entry is gone: the entry is always a
/// folder-repo directory.
#[test]
fn daemon_refuses_a_non_repo_entry() {
    let work = tempfile::Builder::new()
        .prefix("au-bare-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = tempfile::Builder::new()
        .prefix("au-home-")
        .tempdir_in("/tmp")
        .unwrap();
    // A bare directory: content, but no `.arsumbris/repo.yaml`, so not a folder-repo.
    fs::write(work.path().join("a.md"), "---\ntype: note\n---\n").unwrap();
    let entry = fs::canonicalize(work.path()).unwrap();
    let home = fs::canonicalize(home.path()).unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_au"))
        .args(["daemon", "start"])
        .arg(&entry)
        .env("HOME", &home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run daemon start");
    // Exit code 5 = NOT_A_REPO (distinct from 2 env-error / 3 already-serving).
    assert_eq!(
        status.code(),
        Some(5),
        "a non-repo entry is refused with the NOT_A_REPO exit code, got {status:?}"
    );
}

#[test]
fn open_resolves_a_named_workspace_and_serves_it() {
    // `au open <name>` resolves the name through the per-user workspaces index
    // (under a fake $HOME) to a workspace-repo FOLDER, then serves it exactly like
    // `daemon start <folder>`.
    let work = tempfile::Builder::new()
        .prefix("au-open-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = tempfile::Builder::new()
        .prefix("au-home-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = work.path();
    // The entry folder-repo `home`, composing itself plus a nested member `proj`.
    fs::create_dir_all(root.join(".arsumbris")).unwrap();
    fs::write(root.join(".arsumbris/repo.yaml"), "name: home\n").unwrap();
    fs::write(
        root.join(".arsumbris/workspace.yaml"),
        "edit:\n  - home\n  - proj\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("proj/.arsumbris")).unwrap();
    fs::write(root.join("proj/.arsumbris/repo.yaml"), "name: proj\n").unwrap();
    fs::create_dir(root.join("proj/type")).unwrap();
    fs::write(
        root.join("proj/type/note.type.yaml"),
        "fields:\n  x?: String\n",
    )
    .unwrap();
    fs::write(root.join("proj/a.md"), "---\ntype: note\n---\n").unwrap();

    let entry = fs::canonicalize(root).unwrap();
    let home = fs::canonicalize(home.path()).unwrap();

    // The per-user workspaces index names the folder `kb` (an explicit alias, its
    // basename is a random tempdir name). Written under the fake $HOME device root so
    // the run is hermetic.
    let cfg = home.join(".arsumbris/au-engine/config");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(
        cfg.join("workspaces.yaml"),
        format!("workspaces:\n  - name: kb\n    path: {}\n", entry.display()),
    )
    .unwrap();

    // The socket is derived from the RESOLVED folder, so `au open kb` and
    // `daemon start <folder>` bind the same endpoint.
    let socket = socket_under(&home, &entry);

    let mut child = Command::new(env!("CARGO_BIN_EXE_au"))
        .args(["open", "kb"])
        .env("HOME", &home)
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn au open");

    let resp = wait_ready(&socket);
    assert_eq!(resp["result"]["lifecycle"]["ref"], "ready");

    // The named workspace's member type mounted, proving `open` entered the folder.
    let mut client = Client::connect(&socket).expect("connect");
    let types = client.query(&json!({ "read": "types" })).unwrap();
    assert!(
        serde_json::to_string(&types["result"]["types"])
            .unwrap()
            .contains("note"),
        "the resolved workspace's type mounts: {}",
        types["result"]["types"]
    );

    let stop = Command::new(env!("CARGO_BIN_EXE_au"))
        .args(["daemon", "stop"])
        .arg(&entry)
        .env("HOME", &home)
        .stdout(Stdio::null())
        .status()
        .expect("run daemon stop");
    assert!(stop.success(), "stop exits cleanly");
    let status = child.wait().expect("daemon exits");
    assert!(status.success(), "daemon exits cleanly, got {status:?}");
    assert!(!socket.exists(), "socket removed on shutdown");
}

/// The pid file the daemon records beside its socket, `<hash>.pid`.
fn pid_file_under(home: &Path, entry: &Path) -> std::path::PathBuf {
    socket_under(home, entry).with_extension("pid")
}

/// True while `pid` names a live process, via `kill -0` (exit 0 = alive). Keeps
/// the liveness probe out of the test crate's deps, mirroring the binary's own
/// `kill(pid, 0)`.
fn pid_alive(pid: i32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The pid-file reach: a daemon that has NOT bound its socket (booting or wedged)
/// is still reachable by `status` and `stop --force` through its recorded pid,
/// the surfaces the unit tests could not cover.
///
/// Stand in for the not-yet-serving daemon with a detached `sleep`: it holds a
/// live pid with no socket, and being orphaned to init (spawned via `sh &`, its
/// launcher reaped) it is reaped by init when `stop --force` kills it, so the
/// liveness probe sees a true death rather than a zombie this test would own.
#[test]
fn booting_daemon_is_reachable_by_status_start_refusal_and_stop_force() {
    let fx = fixture();
    let root = fs::canonicalize(fx.repo.path()).unwrap();
    let home = fs::canonicalize(fx.home.path()).unwrap();
    let socket = socket_under(&home, &root);
    let pid_file = pid_file_under(&home, &root);

    // A detached `sleep`, orphaned to init, standing in for a daemon that is
    // present but has not bound its socket.
    let launch = Command::new("sh")
        .args(["-c", "sleep 60 >/dev/null 2>&1 & echo $!"])
        .output()
        .expect("spawn detached sleep");
    let pid: i32 = String::from_utf8_lossy(&launch.stdout)
        .trim()
        .parse()
        .expect("sleep pid");
    assert!(pid_alive(pid), "the stand-in daemon is alive");

    // Record its pid where `start` would, with no socket bound.
    fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    fs::write(&pid_file, pid.to_string()).unwrap();
    assert!(!socket.exists(), "no socket: the stand-in is not serving");

    // status: reports a present-but-not-serving daemon, exit 0 (not the exit-4
    // "no daemon running").
    let status = Command::new(env!("CARGO_BIN_EXE_au"))
        .args(["daemon", "status"])
        .arg(&root)
        .env("HOME", &home)
        .output()
        .expect("run daemon status");
    assert_eq!(
        status.status.code(),
        Some(0),
        "a booting daemon is present, so status exits 0, got {:?}\nstdout: {}",
        status.status,
        String::from_utf8_lossy(&status.stdout),
    );
    let out = String::from_utf8_lossy(&status.stdout);
    assert!(
        out.contains("not yet serving") && out.contains(&pid.to_string()),
        "status names the booting daemon and its pid: {out}"
    );

    // start: refuses (exit 3) rather than racing a second cold build.
    let start = Command::new(env!("CARGO_BIN_EXE_au"))
        .args(["daemon", "start"])
        .arg(&root)
        .env("HOME", &home)
        .output()
        .expect("run daemon start");
    assert_eq!(
        start.status.code(),
        Some(3),
        "a live pid holding the entry refuses a new start with ALREADY_SERVING, got {:?}",
        start.status,
    );
    assert!(
        String::from_utf8_lossy(&start.stderr).contains("already running or still booting"),
        "start names the held entry: {}",
        String::from_utf8_lossy(&start.stderr),
    );

    // stop --force: reclaims it by signalling the pid, exit 0, pid file removed.
    let stop = Command::new(env!("CARGO_BIN_EXE_au"))
        .args(["daemon", "stop", "--force"])
        .arg(&root)
        .env("HOME", &home)
        .output()
        .expect("run daemon stop --force");
    assert_eq!(
        stop.status.code(),
        Some(0),
        "stop --force exits cleanly, got {:?}",
        stop.status,
    );
    assert!(!pid_alive(pid), "stop --force killed the stand-in daemon");
    assert!(!pid_file.exists(), "stop --force removed the pid file");
}

/// A stale pid file (its recorded process is gone) reads as genuine absence:
/// `status` exits 4 and reclaims the file, so a dead pid never masquerades as a
/// booting daemon or gets signalled.
#[test]
fn status_reclaims_a_stale_pid_file() {
    let fx = fixture();
    let root = fs::canonicalize(fx.repo.path()).unwrap();
    let home = fs::canonicalize(fx.home.path()).unwrap();
    let pid_file = pid_file_under(&home, &root);

    // i32::MAX is not a live pid, so this file is stale by construction.
    fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    fs::write(&pid_file, i32::MAX.to_string()).unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_au"))
        .args(["daemon", "status"])
        .arg(&root)
        .env("HOME", &home)
        .output()
        .expect("run daemon status");
    assert_eq!(
        status.status.code(),
        Some(4),
        "a stale pid is genuine absence, NOT_RUNNING, got {:?}",
        status.status,
    );
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("no daemon running"),
        "status reports absence: {}",
        String::from_utf8_lossy(&status.stdout),
    );
    assert!(!pid_file.exists(), "the stale pid file is reclaimed");
}

#[test]
fn open_an_unknown_name_is_a_usage_error_naming_the_known_ones() {
    // An unknown workspace name is a loud usage error (exit 2), naming the known
    // workspaces, never a silent empty open.
    let home = tempfile::Builder::new()
        .prefix("au-home-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = fs::canonicalize(home.path()).unwrap();
    let cfg = home.join(".arsumbris/au-engine/config");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(
        cfg.join("workspaces.yaml"),
        "workspaces:\n  - path: /nowhere/kb\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_au"))
        .args(["open", "nope"])
        .env("HOME", &home)
        .output()
        .expect("run au open");
    assert_eq!(
        out.status.code(),
        Some(2),
        "an unknown name is a usage/environment error"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no workspace named 'nope'"),
        "names the missing workspace: {stderr}"
    );
    assert!(
        stderr.contains("known workspaces: kb"),
        "lists the known workspaces: {stderr}"
    );
}
