//! `au daemon start|stop|status <entry>` — the per-workspace daemon process.
//!
//! The entry is a folder-repo DIRECTORY: a repo carrying `.arsumbris/repo.yaml`,
//! composed by its optional `.arsumbris/workspace.yaml`. `start` refuses a
//! non-repo entry (a bare directory, a stray file) with the `NOT_A_REPO` exit
//! code before it binds. One daemon serves one entry over a
//! Unix socket at a short hashed path outside the repo,
//! `~/.arsumbris/au-engine/run/<hash-of-abs-entry-path>.sock` (see
//! [`au_engine::socket_path`]), so a deep repo path can never overrun the
//! socket-path limit and many workspaces in one folder each get their own
//! endpoint. `start` runs in the foreground: it boots the engine, watches every
//! member tree, and serves reads until asked to stop, so a consumer spawns and
//! owns it as a child process. `stop` connects and sends the shutdown control
//! verb; `status` connects and probes readiness. Both report cleanly when no
//! daemon is running.
//!
//! The socket binds only AFTER the cold build, so a daemon that is still booting
//! (or wedged) holds the entry without answering its socket, invisible to a
//! socket-only `stop` / `status`. To close that gap, `start` records its pid in a
//! `<hash>.pid` file beside the socket the instant it begins (see [`pid_path`]),
//! removed on a clean stop. `status` reads it to report a booting daemon ("present
//! but not yet serving") instead of "no daemon running", and `stop --force` reads
//! it to reclaim a booting or wedged daemon by signalling the pid (a graceful
//! window, then SIGTERM, then SIGKILL). Plain `stop` stays socket-only, so it
//! never signals a mid-build daemon. This does NOT close the residual start race
//! (two concurrent starts over a stale entry); that wants a lifetime OS file
//! lock, tracked separately.
//!
//! `start` (when one is already serving) and `status` also warn when the running
//! daemon's `schema_version` differs from this binary's compiled version: a
//! daemon never restarted after a schema bump keeps speaking the old wire and
//! silently corrupts reads. The warning is advisory and leaves exit codes
//! unchanged.
//!
//! Both also surface a DEGRADED workspace: when a declared workspace member or
//! dependency did not resolve to a valid on-disk repo (a stale registry path
//! after a repo move is the common cause), its type-defs are silently absent
//! from the served workspace, so downstream validate / codegen break far from
//! the cause. `start` prints a per-member boot warning after the first build;
//! `status` prints a compact per-code summary. Both are advisory — the workspace
//! opens degraded, so exit codes are unchanged. The signal is scoped to the
//! member/dependency-resolution diagnostic family (see `is_member_resolution_code`),
//! never a blanket warning count: an open-world knowledge base always carries
//! advisory warnings, and a raw count would bury this structural signal.
//!
//! Exit codes are distinct per outcome so a script can branch without parsing
//! stderr: `0` success, `2` usage/environment error, `3` start found one already
//! serving OR still booting, `4` status found none running, `5` start was
//! pointed at an entry that is not a folder-repo. `status` exits `0` for a daemon
//! that is present but still booting (a live pid, no answering socket), and `4`
//! only for genuine absence. `stop` exits `0` whether it stopped a daemon or
//! found none. See the `ENV_ERROR` / `ALREADY_SERVING` / `NOT_RUNNING` /
//! `NOT_A_REPO` constants.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use au_diagnostics::Diagnostic;
use au_engine::{serve, socket_path, Client, Engine, SCHEMA_VERSION};
use serde_json::json;

/// Exit codes for `au daemon`. `0` is success. `2` is a usage or environment
/// error (unresolvable repo, cannot create/watch/bind, an IO error reaching a
/// daemon). The remaining codes name a specific daemon state so a script can
/// branch on it rather than overloading `1`:
/// - `3` — `start` found a daemon already serving the repo.
/// - `4` — `status` found no daemon running.
/// - `5` — `start` was pointed at an entry that is not a folder-repo.
const ENV_ERROR: u8 = 2;
const ALREADY_SERVING: u8 = 3;
const NOT_RUNNING: u8 = 4;
const NOT_A_REPO: u8 = 5;

/// The graceful window before `stop --force` escalates SIGTERM, and again before
/// SIGKILL. Matches au-engine-sdk's `DaemonSupervisor` `DEFAULT_FORCE_AFTER_MS`,
/// so the CLI and the SDK supervisor agree on how long a stop waits.
const FORCE_STOP_WINDOW: Duration = Duration::from_millis(1500);

/// The pid file for an entry, `<hash>.pid` beside the socket in
/// `~/.arsumbris/au-engine/run/`.
///
/// It shares the socket's entry-derived hash name and lives outside the repo, so
/// it exists for the same reasons the socket does (a deep repo path never
/// overruns a path limit, and both are derivable from the entry alone). It
/// records the daemon's pid the instant `start` begins — BEFORE the socket binds,
/// which happens only after the cold build — so `stop --force` and `status` have
/// a handle on a daemon that is still booting or wedged and cannot answer its
/// socket.
fn pid_path(entry: &Path) -> PathBuf {
    socket_path(entry).with_extension("pid")
}

/// The pid recorded in `path`, `None` when the file is absent or unparseable. A
/// garbled file reads as absent, so a corrupt pid never signals a random process.
fn read_recorded_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether `pid` names a live process. `kill(pid, 0)` sends no signal; it only
/// runs the existence and permission check, so `Ok` (0) or `EPERM` means the
/// process is alive (EPERM: alive but owned by another user), and `ESRCH` means
/// it is gone. Unix-only, like the whole daemon (it serves over a Unix socket).
fn pid_alive(pid: i32) -> bool {
    // SAFETY: `kill` with signal 0 performs only error checking — no signal is
    // delivered — so it has no memory-safety hazard; it returns 0 or sets errno.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Send `signal` to `pid`. Unix-only.
fn signal_pid(pid: i32, signal: i32) -> std::io::Result<()> {
    // SAFETY: `kill` takes a pid and a signal number and returns a status; it has
    // no memory-safety hazard.
    let rc = unsafe { libc::kill(pid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Poll until `pid` exits or `timeout` elapses; `true` if it exited. Used by
/// `stop --force` to wait out a graceful shutdown, then a SIGTERM, before
/// escalating.
fn wait_for_exit(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !pid_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A pid file this process owns for the daemon's lifetime.
///
/// Written when `start` begins and removed on drop — a clean shutdown, an
/// early-return error, or a panic — so a gracefully-exiting daemon leaves no
/// stale pid behind. A daemon killed by a signal cannot run `Drop`, so its file
/// lingers; a later `start` / `status` / `stop --force` detects the dead pid
/// (`kill(pid, 0)` → `ESRCH`) and reclaims it. A pid can in principle be reused
/// by an unrelated process between a signal-death and the next check; the force
/// path signals an unverified pid only when the socket is not answering (a
/// booting or wedged daemon, never a cleanly serving one, which the graceful
/// shutdown handles first), shrinking that window; the airtight fix is the
/// lifetime OS file lock.
struct PidFile {
    path: PathBuf,
}

impl PidFile {
    /// Record this process's pid at `path`, creating or overwriting it.
    fn write(path: PathBuf) -> std::io::Result<Self> {
        std::fs::write(&path, std::process::id().to_string())?;
        Ok(PidFile { path })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `au open <name>`: resolve a NAMED workspace through the per-user
/// `~/.arsumbris/au-engine/config/workspaces.yaml` index to its workspace-repo FOLDER, then
/// enter it exactly as `daemon start <folder>`.
///
/// The index only POINTS at folders (it never inlines a selection), so the
/// folder's committed `.arsumbris/workspace.yaml` is always the definition and
/// this only names it — there is no name-lookup spanning both, so a definition
/// never disagrees with an index. An unknown name (or an empty / absent index) is
/// a loud usage error naming the known workspaces, never a silent empty open. A
/// named folder that is missing or not a repo surfaces through `start`'s own
/// entry resolution.
pub fn open(name: &str) -> ExitCode {
    let workspaces = au_engine::repo::load_user_workspaces(
        &au_engine::ConfigSource::User,
        &au_parser::RealFileSystem,
    );
    match workspaces.get(name) {
        Some(path) => start(path),
        None => {
            let mut known: Vec<&str> = workspaces.keys().map(String::as_str).collect();
            known.sort_unstable();
            if known.is_empty() {
                eprintln!(
                    "au open: no workspace named '{name}' — the per-user workspaces index \
                     (~/.arsumbris/au-engine/config/workspaces.yaml) is empty or absent. Add a \
                     '- path: <workspace-repo folder>' entry, or point 'au daemon start' at the \
                     folder directly."
                );
            } else {
                eprintln!(
                    "au open: no workspace named '{name}' — known workspaces: {}",
                    known.join(", ")
                );
            }
            ExitCode::from(ENV_ERROR)
        }
    }
}

/// Boot a daemon over `entry` and serve until a shutdown request arrives.
///
/// Refuses to start a second daemon while one is already serving the repo.
/// The authoritative guard is the bind in `serve`: the OS lets exactly one
/// process bind the socket path, and a stale socket from an ungraceful exit is
/// reclaimed only after a connect-probe confirms nothing is listening. The
/// probe here is a fast, friendly pre-check with a clear message. Blocks for
/// the lifetime of the daemon; on shutdown the socket is removed.
pub fn start(entry: &Path) -> ExitCode {
    let entry = match canonical_entry(entry) {
        Ok(e) => e,
        Err(code) => return code,
    };
    // The device area `~/.arsumbris` (registries, cache, socket) requires `$HOME`.
    // Refuse to start when it is unset rather than binding a socket in a temp dir
    // with no registry, an environment error.
    if let Err(e) = au_engine::device_root() {
        eprintln!("au: cannot serve {}: {e}", entry.display());
        return ExitCode::from(ENV_ERROR);
    }
    // Operation tracing, off unless `AU_TRACE` is set. The guards flush the trace
    // and drain the log on shutdown, so they are held for the daemon's lifetime.
    let _trace_guards = install_tracing(&entry);
    // Preflight: the entry MUST be a folder-repo (a directory carrying a valid,
    // named `.arsumbris/repo.yaml`). Refuse a non-repo entry up front with a
    // distinct exit code, before binding the socket or booting the engine, rather
    // than serving an empty repo the build would refuse.
    if let Err(e) = au_engine::verify_entry(&entry) {
        eprintln!("au: cannot serve {}: {e}", entry.display());
        return ExitCode::from(NOT_A_REPO);
    }
    let socket = socket_path(&entry);

    // Friendly pre-check: if a live daemon answers, say so up front. The bind
    // in `serve` is the real mutual-exclusion primitive; this only improves the
    // message in the common case. A daemon already serving may be stale: if it
    // speaks an older wire than this binary compiles, warn loudly, since the skew
    // silently corrupts reads otherwise.
    if let Some(resp) = probe_ready(&socket) {
        eprintln!("au: a daemon is already serving {}", entry.display());
        warn_on_schema_skew(&resp, &entry);
        return ExitCode::from(ALREADY_SERVING);
    }

    // The socket lives outside the repo now, under `~/.arsumbris/au-engine/run/`, so
    // ensure that directory exists before `serve` binds into it. Restrict it to
    // the owning user (`0o700`): the socket path is world-derivable (a non-secret
    // hash of a non-secret entry path) and the wire is unauthenticated, so the
    // socket itself is `0o600` (in `serve`); tightening the dir too means the
    // protection does not rely on inherited `~` permissions.
    if let Some(dir) = socket.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("au: cannot create socket dir {}: {e}", dir.display());
            return ExitCode::from(ENV_ERROR);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }

    // The socket did not answer above, so nothing is SERVING this entry. But a
    // daemon may still be BOOTING (or wedged): the socket binds only after the
    // cold build, so a slow build holds the entry without answering. A live
    // recorded pid is that daemon — refuse rather than race a second cold build,
    // and point at the force path for a wedged one. A stale pid (its process is
    // gone) is reclaimed by overwriting the file below.
    let pid_file_path = pid_path(&entry);
    if let Some(pid) = read_recorded_pid(&pid_file_path) {
        if pid_alive(pid) {
            // The socket probe above did not answer, so this is almost always a
            // still-booting daemon; but a transient probe failure against a
            // genuinely serving one falls through here too, so the message names
            // both rather than asserting "booting".
            eprintln!(
                "au: a daemon is already running or still booting for {} (pid {pid}, socket not \
                 answering); wait for it to be ready, or reclaim a wedged one with \
                 'au daemon stop --force'",
                entry.display()
            );
            return ExitCode::from(ALREADY_SERVING);
        }
    }
    // Record our pid before the slow boot, so `stop --force` / `status` have a
    // handle from this instant on, not only once the socket binds. Held for the
    // daemon's lifetime; dropped (removing the file) on a clean stop or any
    // early return below.
    let _pid_file = match PidFile::write(pid_file_path) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("au: cannot write pid file: {e}");
            return ExitCode::from(ENV_ERROR);
        }
    };

    // Set up the engine (no IO yet) so its workspace root is known: the entry
    // folder-repo directory. The socket, watcher, and saga markers key off it.
    // The daemon is the one caller that reads the real per-user config: the
    // registry is how a user's scattered members and registered deps resolve.
    let mut engine = Engine::new(&entry, au_engine::ConfigSource::User);
    let root = engine.handle().root().to_path_buf();

    // Roll back any saga interrupted by a crash before building or serving, so
    // the IR reflects the recovered state and the watcher never sees the rollback
    // as an external edit.
    let recovery = au_engine::recover_crashed_saga(&root);
    if let Some(r) = &recovery.recovered {
        println!(
            "au: recovered an interrupted mutation {} ({} reverted, {} restored)",
            r.mutation_id,
            r.reverted.len(),
            r.restored.len()
        );
    }
    // An un-rolled-back saga needs manual repair; surface it on every start until
    // an operator clears the sentinel.
    for f in &recovery.failures {
        eprintln!(
            "au: WARNING an interrupted mutation could not be fully rolled back, \
             manual repair needed (mutation {}):\n{}",
            f.mutation_id,
            f.detail.trim_end()
        );
    }

    if let Err(e) = engine.watch() {
        eprintln!("au: cannot watch {}: {e}", root.display());
        return ExitCode::from(ENV_ERROR);
    }
    // Build once up front so the ref is Ready when the first status lands,
    // rather than waiting for a file event.
    engine.rebuild();

    // A declared member or dependency that did not resolve leaves the workspace
    // degraded: its type-defs are silently absent. The engine already emits the
    // member-resolution diagnostics; surface them once here so a boot over a
    // stale registry is loud, not silent. Advisory — the daemon serves anyway.
    if let Some(report) =
        degraded_boot_report(&engine.read_diagnostics().value().unwrap_or_default())
    {
        eprintln!("{report}");
    }

    let server = match serve(engine.handle(), &socket) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("au: cannot bind {}: {e}", socket.display());
            return ExitCode::from(ENV_ERROR);
        }
    };
    println!(
        "au: daemon serving {} at {}",
        root.display(),
        socket.display()
    );

    server.wait_for_shutdown();
    // Dropping `server` removes the socket; `engine` drops its watcher.
    println!("au: daemon stopped");
    ExitCode::SUCCESS
}

/// Ask the daemon serving `entry` to shut down, over the socket. With `force`,
/// also reclaim a booting or wedged daemon that has not bound its socket, by
/// signalling its recorded pid (a graceful window, then SIGTERM, then SIGKILL).
/// Reports cleanly when nothing is running, and exits `0` either way.
pub fn stop(entry: &Path, force: bool) -> ExitCode {
    let entry = match canonical_entry(entry) {
        Ok(e) => e,
        Err(code) => return code,
    };
    let socket = socket_path(&entry);

    // Graceful path: ask a SERVING daemon to shut down over the socket. `served`
    // records whether one acked, so the force path knows whether to wait out a
    // graceful shutdown before escalating to a signal.
    let served = match Client::connect(&socket) {
        Ok(mut client) => match client.query(&json!({ "read": "shutdown" })) {
            Ok(_) => {
                println!("au: shutdown requested for {}", entry.display());
                true
            }
            Err(e) => {
                eprintln!("au: shutdown request failed: {e}");
                // Without --force a failed request is the end of the line; with
                // it, fall through to signal the recorded pid.
                if !force {
                    return ExitCode::from(ENV_ERROR);
                }
                false
            }
        },
        // An absent socket or a stale one that refuses the connection means
        // nothing is SERVING: the goal may still be met by the force path (a
        // booting/wedged daemon), or already met (nothing runs at all). Any
        // other error (a permission or IO failure on an existing socket) is a
        // real problem, not absence — report it unless we can still force.
        Err(e) if e.kind() == ErrorKind::NotFound || e.kind() == ErrorKind::ConnectionRefused => {
            false
        }
        Err(e) => {
            eprintln!("au: cannot reach daemon for {}: {e}", entry.display());
            if !force {
                return ExitCode::from(ENV_ERROR);
            }
            false
        }
    };

    if !force {
        if !served {
            println!("au: no daemon running for {}", entry.display());
        }
        return ExitCode::SUCCESS;
    }

    // Force path: reach a booting/wedged daemon the socket could not, via its
    // recorded pid.
    let pid_file_path = pid_path(&entry);
    let Some(pid) = read_recorded_pid(&pid_file_path) else {
        // No pid recorded: a graceful stop already handled a serving daemon, or
        // nothing was running.
        if !served {
            println!("au: no daemon running for {}", entry.display());
        }
        return ExitCode::SUCCESS;
    };
    if !pid_alive(pid) {
        let _ = std::fs::remove_file(&pid_file_path); // a stale pid, reclaim it
        if !served {
            println!("au: no daemon running for {}", entry.display());
        }
        return ExitCode::SUCCESS;
    }

    // A graceful shutdown was requested above; give it the window to take effect
    // before signalling, so a serving daemon still exits cleanly under --force.
    if served && wait_for_exit(pid, FORCE_STOP_WINDOW) {
        let _ = std::fs::remove_file(&pid_file_path);
        println!("au: daemon stopped for {}", entry.display());
        return ExitCode::SUCCESS;
    }

    // Escalate: SIGTERM, wait, then SIGKILL. Abrupt termination is safe — the
    // next `start` runs `recover_crashed_saga`, rolling back any interrupted
    // mutation.
    let _ = signal_pid(pid, libc::SIGTERM);
    if !wait_for_exit(pid, FORCE_STOP_WINDOW) {
        let _ = signal_pid(pid, libc::SIGKILL);
        let _ = wait_for_exit(pid, FORCE_STOP_WINDOW);
    }
    let _ = std::fs::remove_file(&pid_file_path);
    println!(
        "au: force-stopped daemon for {} (pid {pid})",
        entry.display()
    );
    ExitCode::SUCCESS
}

/// Report whether a daemon is serving `entry`, and its lifecycle if so.
pub fn status(entry: &Path) -> ExitCode {
    let entry = match canonical_entry(entry) {
        Ok(e) => e,
        Err(code) => return code,
    };
    let socket = socket_path(&entry);

    match probe_ready(&socket) {
        Some(resp) => {
            let ready = resp["ready"].as_bool().unwrap_or(false);
            let engine = resp["result"]["lifecycle"]["engine"]
                .as_str()
                .unwrap_or("?");
            let refstate = resp["result"]["lifecycle"]["ref"].as_str().unwrap_or("?");
            let version = resp["version"].as_u64();
            println!(
                "au: daemon running for {} (engine={engine}, ref={refstate}, ready={ready}, version={})",
                entry.display(),
                version.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string()),
            );
            warn_on_schema_skew(&resp, &entry);
            // Surface a degraded workspace: a declared member / dependency that
            // did not resolve. Advisory, so the exit code stays SUCCESS.
            if let Some(line) = degraded_status_line(&socket) {
                eprintln!("{line}");
            }
            ExitCode::SUCCESS
        }
        None => {
            // Nothing answered the socket, so nothing is SERVING. But a daemon
            // may be present and still BOOTING (the socket binds only after the
            // cold build) or wedged. A live recorded pid is that daemon —
            // surface it rather than reporting absence, and exit `0` since a
            // daemon IS present for this entry. A dead pid is a stale file:
            // reclaim it and report genuine absence.
            let pid_file_path = pid_path(&entry);
            match read_recorded_pid(&pid_file_path) {
                Some(pid) if pid_alive(pid) => {
                    println!(
                        "au: daemon present but not yet serving for {} (booting, pid {pid}); \
                         reclaim a wedged one with 'au daemon stop --force'",
                        entry.display()
                    );
                    ExitCode::SUCCESS
                }
                Some(_) => {
                    let _ = std::fs::remove_file(&pid_file_path);
                    println!("au: no daemon running for {}", entry.display());
                    ExitCode::from(NOT_RUNNING)
                }
                None => {
                    println!("au: no daemon running for {}", entry.display());
                    ExitCode::from(NOT_RUNNING)
                }
            }
        }
    }
}

/// Print the schema-skew warning to stderr when one applies. A thin wrapper over
/// `schema_skew_warning`, which holds the testable decision.
fn warn_on_schema_skew(resp: &serde_json::Value, root: &Path) {
    if let Some(msg) = schema_skew_warning(resp, root) {
        eprintln!("{msg}");
    }
}

/// The warning a running daemon's frame warrants when it speaks a different wire
/// schema than this binary compiles, or `None` when they agree (or the frame
/// carries no `schema_version` to compare). Every frame carries `schema_version`,
/// so the probe response reveals the skew; the common cause is a long-running
/// daemon never restarted after a rebuild that bumped the schema. The skew
/// silently corrupts reads, so the message names both versions and the fix.
fn schema_skew_warning(resp: &serde_json::Value, root: &Path) -> Option<String> {
    let daemon = resp["schema_version"].as_u64()?;
    let compiled = u64::from(SCHEMA_VERSION);
    (daemon != compiled).then(|| {
        format!(
            "au: WARNING: a stale daemon (schema v{daemon}) is serving {}; \
             this binary speaks v{compiled} — stop and restart it",
            root.display(),
        )
    })
}

/// Connect and issue the `lifecycle` probe, returning the response when a daemon
/// answers. `None` when the socket is absent, stale, or unreachable.
fn probe_ready(socket: &Path) -> Option<serde_json::Value> {
    let mut client = Client::connect(socket).ok()?;
    client.query(&json!({ "read": "lifecycle" })).ok()
}

/// True when `code` names a diagnostic that a declared workspace member or
/// dependency did not resolve — its type-defs are silently absent from the
/// served workspace, so downstream validate / codegen break far from the cause.
///
/// This is deliberately the member/dependency-RESOLUTION family, not every
/// warning. An open-world knowledge base always carries advisory warnings
/// (dangling links, candidate hints, drift); a blanket count would cry wolf on a
/// healthy workspace and bury this structural signal. Kept in one list so
/// `start` and `status` surface the same set, keyed off the engine's own code
/// constants so a rename tracks here automatically.
fn is_member_resolution_code(code: &str) -> bool {
    use au_engine::repo::{
        DEPENDENCY_CACHE_MISS, DISCOVER_MEMBER_UNMOUNTED, EDIT_MEMBER_READ_ONLY,
        EDIT_MEMBER_UNMOUNTED, MEMBER_AT_RESERVED_ROOT, PEER_UNMOUNTED,
        WORKSPACE_MEMBER_UNWALKABLE,
    };
    [
        EDIT_MEMBER_UNMOUNTED,
        EDIT_MEMBER_READ_ONLY,
        DISCOVER_MEMBER_UNMOUNTED,
        PEER_UNMOUNTED,
        WORKSPACE_MEMBER_UNWALKABLE,
        MEMBER_AT_RESERVED_ROOT,
        DEPENDENCY_CACHE_MISS,
    ]
    .iter()
    .any(|c| c.as_str() == code)
}

/// The multi-line boot warning `start` prints when the freshly-built workspace
/// carries member-resolution diagnostics, or `None` when every declared member
/// and dependency resolved. Lists each diagnostic's own message, which already
/// names the member and the cause, so the operator sees the dead paths directly.
/// Advisory: `start` prints it and serves anyway.
fn degraded_boot_report(diags: &[Diagnostic]) -> Option<String> {
    let degraded: Vec<&Diagnostic> = diags
        .iter()
        .filter(|d| is_member_resolution_code(d.code.as_str()))
        .collect();
    if degraded.is_empty() {
        return None;
    }
    let mut out = format!(
        "au: WARNING workspace opened degraded — {} declared member(s)/dependency(ies) did not \
         resolve; their types are absent from the served workspace:",
        degraded.len()
    );
    for d in degraded {
        out.push_str(&format!("\n  [{}] {}", d.code.as_str(), d.message));
    }
    Some(out)
}

/// Query the running daemon's `diagnostic_counts` and return the compact
/// member-resolution summary line, or `None` when the read fails or nothing is
/// degraded. Thin IO wrapper over `degraded_counts_line`, the testable decision.
fn degraded_status_line(socket: &Path) -> Option<String> {
    let mut client = Client::connect(socket).ok()?;
    let resp = client.query(&json!({ "read": "diagnostic_counts" })).ok()?;
    degraded_counts_line(&resp["result"]["diagnostic_counts"]["by_code"])
}

/// The one-line `status` summary from a `diagnostic_counts` `by_code` map (code →
/// count), or `None` when no member-resolution diagnostic is present. Compact by
/// design: `status` is a repeatable probe, so it reports per-code counts and
/// leaves the per-member detail to the `diagnostics` read. Grouping by code means
/// it never parses a member name out of a message.
fn degraded_counts_line(by_code: &serde_json::Value) -> Option<String> {
    let map = by_code.as_object()?;
    let mut parts: Vec<(String, u64)> = map
        .iter()
        .filter(|(code, _)| is_member_resolution_code(code))
        .map(|(code, n)| (code.clone(), n.as_u64().unwrap_or(0)))
        .filter(|(_, n)| *n > 0)
        .collect();
    if parts.is_empty() {
        return None;
    }
    parts.sort();
    let total: u64 = parts.iter().map(|(_, n)| n).sum();
    let breakdown = parts
        .iter()
        .map(|(code, n)| format!("{code}: {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "au: WARNING workspace degraded — {total} declared member(s)/dependency(ies) did not \
         resolve ({breakdown})"
    ))
}

/// Canonicalize the entry path (the folder-repo directory) so the daemon and any
/// consumer hash an identical string and derive the same socket path. A missing
/// entry is a usage error.
fn canonical_entry(entry: &Path) -> Result<PathBuf, ExitCode> {
    std::fs::canonicalize(entry).map_err(|e| {
        eprintln!("au: cannot resolve entry {}: {e}", entry.display());
        ExitCode::from(ENV_ERROR)
    })
}

/// The flush guards for an active operation trace, held for the daemon's
/// lifetime. Dropping them flushes the Perfetto trace and drains the timing log.
struct TraceGuards {
    _chrome: tracing_chrome::FlushGuard,
    _appender: tracing_appender::non_blocking::WorkerGuard,
}

/// Install the operation-tracing subscriber when `AU_TRACE` is set, writing a
/// Perfetto trace and a timing log under `<entry>/.arsumbris/au-engine/logs/trace/`,
/// each named by the run's start time. `None` when tracing is off, the default,
/// where no subscriber is installed and the span macros cost nothing.
///
/// See [[spec - operation tracing - env-gated spans at the seams emit a perfetto trace and a timing log]].
fn install_tracing(entry: &Path) -> Option<TraceGuards> {
    use tracing_subscriber::prelude::*;

    if std::env::var_os("AU_TRACE").is_none() {
        return None;
    }
    let dir = entry
        .join(".arsumbris")
        .join("au-engine")
        .join("logs")
        .join("trace");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("au: AU_TRACE set but cannot create {}: {e}", dir.display());
        return None;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let (chrome, chrome_guard) = tracing_chrome::ChromeLayerBuilder::new()
        .file(dir.join(format!("trace-{stamp}.json")))
        .include_args(true)
        .build();

    let log_file = match std::fs::File::create(dir.join(format!("timing-{stamp}.log"))) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("au: AU_TRACE set but cannot open the timing log: {e}");
            return None;
        }
    };
    let (log_writer, appender_guard) = tracing_appender::non_blocking(log_file);
    let timing = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(log_writer)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE);

    // INFO floor: the engine's operation spans are INFO, while dependency crates
    // (`globset`, `ignore`) emit DEBUG events. The floor keeps the trace and log
    // to the engine's own operations, not third-party noise.
    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::INFO)
        .with(chrome)
        .with(timing)
        .init();
    eprintln!("au: AU_TRACE on, tracing operations to {}", dir.display());
    Some(TraceGuards {
        _chrome: chrome_guard,
        _appender: appender_guard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matching_schema_warns_nothing() {
        let resp = json!({ "schema_version": SCHEMA_VERSION, "ready": true });
        assert!(schema_skew_warning(&resp, Path::new("/v")).is_none());
    }

    #[test]
    fn older_daemon_schema_warns() {
        let stale = u64::from(SCHEMA_VERSION) - 1;
        let resp = json!({ "schema_version": stale, "ready": true });
        let msg = schema_skew_warning(&resp, Path::new("/v")).expect("skew warns");
        assert!(
            msg.contains(&format!("schema v{stale}")),
            "names daemon version: {msg}"
        );
        assert!(
            msg.contains(&format!("v{SCHEMA_VERSION}")),
            "names compiled version: {msg}"
        );
        assert!(msg.contains("/v"), "names the repo: {msg}");
    }

    #[test]
    fn absent_schema_field_warns_nothing() {
        // A frame without `schema_version` gives nothing to compare.
        let resp = json!({ "ready": true });
        assert!(schema_skew_warning(&resp, Path::new("/v")).is_none());
    }

    use au_diagnostics::{ByteRange, Severity, Span};

    fn member_diag(code: au_diagnostics::DiagnosticCode, message: &str) -> Diagnostic {
        Diagnostic {
            code,
            severity: Severity::Warning,
            span: Span::new(".arsumbris/workspace.yaml", ByteRange::new(0, 5)),
            message: message.into(),
            related: vec![],
            fix: None,
        }
    }

    #[test]
    fn member_resolution_code_matches_the_family_only() {
        assert!(is_member_resolution_code(
            au_engine::repo::EDIT_MEMBER_UNMOUNTED.as_str()
        ));
        assert!(is_member_resolution_code(
            au_engine::repo::PEER_UNMOUNTED.as_str()
        ));
        // An ordinary open-world advisory is NOT degradation.
        assert!(!is_member_resolution_code("navigational-target-not-found"));
        assert!(!is_member_resolution_code("duplicate-type-def"));
    }

    #[test]
    fn boot_report_is_none_without_member_diagnostics() {
        assert!(degraded_boot_report(&[]).is_none());
        let unrelated = member_diag(
            au_diagnostics::DiagnosticCode::from_static("navigational-target-not-found"),
            "dangling link",
        );
        assert!(degraded_boot_report(&[unrelated]).is_none());
    }

    #[test]
    fn boot_report_counts_and_names_each_degraded_member() {
        let diags = vec![
            member_diag(
                au_engine::repo::EDIT_MEMBER_UNMOUNTED,
                "workspace edit member 'agent-tools' resolves to nothing; the workspace opens degraded",
            ),
            member_diag(
                au_engine::repo::EDIT_MEMBER_UNMOUNTED,
                "workspace edit member 'agent-tools-sdk' resolves to nothing; the workspace opens degraded",
            ),
        ];
        let report = degraded_boot_report(&diags).expect("member diagnostics warn");
        assert!(
            report.contains("2 declared member"),
            "names the count: {report}"
        );
        assert!(
            report.contains("agent-tools'"),
            "names the first member: {report}"
        );
        assert!(
            report.contains("agent-tools-sdk'"),
            "names the second member: {report}"
        );
        assert!(
            report.contains("edit-member-unmounted"),
            "carries the code: {report}"
        );
    }

    #[test]
    fn counts_line_is_none_without_member_codes() {
        assert!(degraded_counts_line(&json!({})).is_none());
        // Non-degradation codes with counts are ignored.
        assert!(degraded_counts_line(&json!({ "navigational-target-not-found": 12 })).is_none());
        // A missing / non-object by_code map is tolerated.
        assert!(degraded_counts_line(&serde_json::Value::Null).is_none());
    }

    #[test]
    fn counts_line_totals_and_breaks_down_by_code() {
        let by_code = json!({
            "edit-member-unmounted": 2,
            "peer-unmounted": 1,
            "navigational-target-not-found": 7,
        });
        let line = degraded_counts_line(&by_code).expect("member codes warn");
        assert!(
            line.contains("3 declared member"),
            "sums only the family: {line}"
        );
        assert!(
            line.contains("edit-member-unmounted: 2"),
            "breakdown: {line}"
        );
        assert!(line.contains("peer-unmounted: 1"), "breakdown: {line}");
        assert!(
            !line.contains("navigational-target-not-found"),
            "excludes open-world advisories: {line}"
        );
    }

    // --- pid file / liveness helpers (B: reach a booting/wedged daemon) ---

    #[test]
    fn pid_path_sits_beside_the_socket_with_a_pid_extension() {
        let entry = Path::new("/some/entry/repo");
        assert_eq!(pid_path(entry), socket_path(entry).with_extension("pid"));
        // Same entry-derived stem as the socket, so both are derivable alone.
        assert_eq!(
            pid_path(entry).file_stem(),
            socket_path(entry).file_stem(),
            "shares the socket's hashed stem"
        );
    }

    #[test]
    fn pid_alive_is_true_for_our_own_process() {
        let me = std::process::id() as i32;
        assert!(pid_alive(me), "the running test process is alive");
    }

    #[test]
    fn pid_alive_is_false_for_an_impossible_pid() {
        // i32::MAX is not a live pid, so kill(pid, 0) → ESRCH → not alive.
        assert!(!pid_alive(i32::MAX));
    }

    #[test]
    fn read_recorded_pid_roundtrips_and_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.pid");
        assert_eq!(read_recorded_pid(&path), None, "absent file reads as None");
        std::fs::write(&path, "4821\n").unwrap();
        assert_eq!(
            read_recorded_pid(&path),
            Some(4821),
            "parses, trims newline"
        );
        std::fs::write(&path, "not-a-pid").unwrap();
        assert_eq!(
            read_recorded_pid(&path),
            None,
            "garbage reads as None so it never signals a random process"
        );
    }

    #[test]
    fn pid_file_writes_our_pid_and_drop_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.pid");
        {
            let _guard = PidFile::write(path.clone()).expect("write pid file");
            assert_eq!(
                read_recorded_pid(&path),
                Some(std::process::id() as i32),
                "records our own pid"
            );
        }
        assert!(!path.exists(), "drop removes the file on a clean exit");
    }

    #[test]
    fn wait_for_exit_returns_true_immediately_for_a_dead_pid() {
        // A never-live pid is already "exited", so this returns true well within
        // the window (it must not block for the whole timeout).
        let start = Instant::now();
        assert!(wait_for_exit(i32::MAX, Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(1), "does not block");
    }
}
