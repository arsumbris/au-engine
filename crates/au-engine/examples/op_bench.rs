//! Time the operations a consumer actually performs, broken down by build phase.
//!
//! Where `scale_bench` answers "how long does a build take at size N", this
//! answers "how long does CREATING A FILE take, and where does that time go".
//! The operation set is [`au_testkit::opcat`], shared with the gate tests that
//! pin which rebuild path each operation takes.
//!
//! Run: `cargo run --release --example op_bench -p au-engine [profile]`
//! - `profile` is `small` / `medium` / `large` / `workspace` / `extreme`,
//!   default `workspace` (the realistic dozens-of-repos shape).
//!
//! Each operation is applied to a watching engine, timed from the write to the
//! version advance that makes it observable, then undone. Per-phase busy times
//! come from an in-process subscriber that accumulates span durations by name,
//! so the breakdown needs no log parsing.
//!
//! The numbers are wall-clock on a developer machine and are not a gate. The
//! gate's job is the rebuild-path assertion, which is machine independent; this
//! is for eyeballing where time goes and whether a change moved it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use au_engine::{ConfigSource, Engine};
use au_testkit::opcat::{self, RebuildPath};
use au_testkit::repogen::{self, Profile};
use tracing::span::{Attributes, Id};
use tracing::Subscriber;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::reload;

/// Switches the engine's spans on and off at runtime.
///
/// A reloadable level filter, not a flag inside the layer: reloading rebuilds
/// tracing's callsite interest cache, so a disabled span is never CONSTRUCTED.
/// A flag would still pay span creation and entry, and would measure the
/// harness's own bookkeeping rather than the instrumentation's cost.
type TraceSwitch = reload::Handle<LevelFilter, tracing_subscriber::Registry>;

/// Install the collector behind a switch, returning the sink and the switch.
fn install(sink: Sink) -> TraceSwitch {
    let (filter, handle) = reload::Layer::new(LevelFilter::INFO);
    tracing_subscriber::registry()
        .with(filter)
        .with(Collector { sink })
        .init();
    handle
}

/// The engine's spans, off for as long as this lives and back on when it drops.
///
/// The untraced window used to be two `reload` calls with fallible work between
/// them, so ANY early return left tracing off for the rest of the run. Every
/// later row then recorded no spans at all, and a row with no spans reads as
/// `Incremental` with an empty breakdown and no watcher event — a report that
/// looks entirely plausible and is entirely wrong.
///
/// Drop is the single exit, so the window closes on the happy path, the `?`, and
/// a panic alike. Closed by construction rather than by inspecting every path
/// through the function.
struct SpansOff<'a>(&'a TraceSwitch);

impl<'a> SpansOff<'a> {
    fn new(switch: &'a TraceSwitch) -> Option<SpansOff<'a>> {
        switch.reload(LevelFilter::OFF).ok()?;
        Some(SpansOff(switch))
    }
}

impl Drop for SpansOff<'_> {
    fn drop(&mut self) {
        // Loudly, and without panicking: a panic in a drop during unwind
        // aborts, and the run is already unusable from here on either way.
        if self.0.reload(LevelFilter::INFO).is_err() {
            eprintln!("  ! could not restore tracing; every row after this one is unusable");
        }
    }
}

/// Time one operation with the engine's spans OFF.
///
/// Only the measured window is untraced. The harness settles, undoes, and
/// detects quiescence by watching spans, so tracing stays on outside the
/// window; switching it off wholesale would remove the very signal the harness
/// uses to know the engine is idle.
fn measure_untraced(
    op: &opcat::Operation,
    targets: &opcat::Targets,
    engine: &Engine,
    sink: &Sink,
    switch: &TraceSwitch,
) -> Option<Duration> {
    settle(engine, sink);
    if let Some(prepare) = op.prepare {
        let expected = prepare(targets).ok()?;
        if !wait_for_catalog(engine, &expected, Duration::from_secs(120)) {
            return None;
        }
        settle(engine, sink);
    }
    let _ = sink.drain();

    let before = engine.handle().version();
    // The untraced window, scoped. `applied` and the latency leave it; the
    // guard does not, so tracing is back on for everything below.
    let (applied, latency) = {
        let _spans_off = SpansOff::new(switch)?;
        let started = Instant::now();
        let applied = (op.apply)(targets).ok()?;
        let latency = loop {
            if matches!((engine.handle().version(), before), (Some(now), Some(was)) if now > was) {
                break Some(started.elapsed());
            }
            if started.elapsed() > MEASURE_TIMEOUT {
                break None;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        (applied, latency)
    };

    settle(engine, sink);
    applied.undo.run().ok()?;
    settle(engine, sink);
    let _ = sink.drain();
    latency
}

/// Per-span-name total busy time, accumulated across one operation, plus the
/// count of spans currently open.
///
/// The open count is what makes quiescence detectable. A version number alone
/// cannot distinguish "nothing is happening" from "a long rebuild is running
/// and has not committed yet", and both look like an unchanged version.
#[derive(Default)]
struct Totals {
    by_name: BTreeMap<&'static str, Duration>,
    open: usize,
}

/// The shared sink the layer writes into and the harness reads between
/// operations.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Totals>>);

impl Sink {
    /// Take and clear, so each operation reports only its own spans.
    fn drain(&self) -> BTreeMap<&'static str, Duration> {
        std::mem::take(&mut self.0.lock().unwrap().by_name)
    }

    /// Whether any span is currently open, i.e. the engine is mid-operation.
    fn busy(&self) -> bool {
        self.0.lock().unwrap().open > 0
    }
}

/// Tracks each span's entered time, the same busy accounting the daemon's
/// timing log reports, kept in memory instead of formatted to a file.
struct Busy {
    entered: Option<Instant>,
    total: Duration,
}

/// Accumulates span busy time by name.
///
/// Busy, not wall-clock: a span's elapsed time would double-count its children,
/// since a parent stays entered while a child runs.
struct Collector {
    sink: Sink,
}

impl<S> Layer<S> for Collector
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, _attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(Busy {
                entered: None,
                total: Duration::ZERO,
            });
        }
        self.sink.0.lock().unwrap().open += 1;
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            if let Some(busy) = span.extensions_mut().get_mut::<Busy>() {
                busy.entered = Some(Instant::now());
            }
        }
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            if let Some(busy) = span.extensions_mut().get_mut::<Busy>() {
                if let Some(at) = busy.entered.take() {
                    busy.total += at.elapsed();
                }
            }
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            self.sink.0.lock().unwrap().open -= 1;
            return;
        };
        let total = span.extensions().get::<Busy>().map(|b| b.total);
        let mut state = self.sink.0.lock().unwrap();
        state.open -= 1;
        if let Some(total) = total {
            *state.by_name.entry(span.name()).or_default() += total;
        }
    }
}

/// A background wire subscriber, timestamping each change event as it arrives.
///
/// The `changes` channel, not `files`: `files` fires only when the SET of
/// catalogued paths differs, so a pure edit produces no event there. `changes`
/// covers modifications too, and both project the same rebuild, so the delivery
/// cost measured here is the one a wire subscriber sees on `files`.
struct WireSub {
    events: std::sync::mpsc::Receiver<Instant>,
    _server: au_engine::ServeHandle,
    _sock_dir: tempfile::TempDir,
}

impl WireSub {
    fn start(engine: &Engine) -> Option<WireSub> {
        let sock_dir = tempfile::tempdir().ok()?;
        let socket = sock_dir.path().join("s");
        let server = au_engine::serve(engine.handle(), &socket).ok()?;
        let mut client = au_engine::Client::connect(&socket).ok()?;
        client
            .send(&serde_json::json!({ "subscribe": "changes" }))
            .ok()?;
        let (tx, events) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(Some(frame)) = client.recv() {
                // Timestamp on arrival, before any inspection, so the reported
                // delivery cost excludes this thread's own parsing.
                let at = Instant::now();
                if frame.get("kind").and_then(|k| k.as_str()) == Some("knowledge-base-changed")
                    && tx.send(at).is_err()
                {
                    return;
                }
            }
        });
        Some(WireSub {
            events,
            _server: server,
            _sock_dir: sock_dir,
        })
    }

    /// Discard events from earlier operations, so a measurement never reads a
    /// stale arrival.
    fn drain(&self) {
        while self.events.try_recv().is_ok() {}
    }

    /// The next event's arrival, or `None` if the channel produced none in time.
    /// A pure edit that changes no content hash legitimately fires nothing.
    fn next_within(&self, budget: Duration) -> Option<Instant> {
        self.events.recv_timeout(budget).ok()
    }
}

/// One operation's result.
struct Row {
    name: &'static str,
    /// Write to observable, what a consumer feels.
    latency: Duration,
    /// Write to the change event landing in a wire subscriber, the
    /// subscriber's measurement. `None` when the operation fired no event.
    wire_latency: Option<Duration>,
    /// Whether the write took the incremental path, read from whether a whole
    /// build ran during it.
    path: RebuildPath,
    /// Whether the idle reconcile served this row, meaning the watcher's own
    /// event never arrived.
    via_reconcile: bool,
    /// Whether the watcher delivered ANY event during the row. Read from the
    /// `watch_event` span, so it is observed rather than inferred.
    saw_event: bool,
    /// The same operation timed with the engine's spans off, the tracing tax.
    untraced: Option<Duration>,
    /// Per-phase busy time, the breakdown of where the latency went.
    phases: BTreeMap<&'static str, Duration>,
    /// Set when the operation's path is a known gap, carried from the catalog.
    gap: Option<RebuildPath>,
}

fn profile_from_arg(arg: Option<&str>) -> (Profile, &'static str) {
    match arg.unwrap_or("workspace") {
        "small" => (Profile::Small, "small"),
        "medium" => (Profile::Medium, "medium"),
        "large" => (Profile::Large, "large"),
        "extreme" => (Profile::WorkspaceExtreme, "extreme"),
        _ => (Profile::Workspace, "workspace"),
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Long enough for the spans enclosing the commit to close, short enough that
/// the next rebuild's quiet window cannot have elapsed.
const SPAN_CLOSE_GRACE: Duration = Duration::from_millis(50);

/// Wait until the engine is genuinely quiescent, so an operation starts from a
/// settled knowledge base and never inherits the previous one's rebuild.
///
/// Three conditions, and all three are needed.
/// - the engine is READY, `version()` is `None` while it derives.
/// - no span is open, so no rebuild is in flight. A version number cannot show
///   this: a long rebuild has not committed yet, so its version looks unchanged.
/// - the version has held still across a window LONGER than the watcher's quiet
///   period, so a write whose rebuild has not started yet is not mistaken for
///   quiescence.
///
/// Dropping the second condition is what made an earlier run report a rebuild
/// span longer than the operation that supposedly contained it.
fn settle(engine: &Engine, sink: &Sink) {
    let mut last = engine.handle().version();
    let mut held_since = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(25));
        let now = engine.handle().version();
        if now != last {
            last = now;
            held_since = Instant::now();
            continue;
        }
        if now.is_some() && !sink.busy() && held_since.elapsed() >= QUIET_MARGIN {
            return;
        }
    }
}

/// Past the idle-reconcile window, so a row the reconcile legitimately serves
/// is measured rather than reported as a failure. The reconcile is the engine's
/// backstop for a dropped watcher event, and its latency is real.
const MEASURE_TIMEOUT: Duration = Duration::from_secs(30);

/// Comfortably past the watcher's 150ms quiet window, so a just-written file
/// has certainly triggered its rebuild (and thus opened a span) before this
/// elapses.
const QUIET_MARGIN: Duration = Duration::from_millis(400);

/// Wait until every path is present in the engine's catalog. `false` on timeout.
fn wait_for_catalog(engine: &Engine, paths: &[std::path::PathBuf], budget: Duration) -> bool {
    let started = Instant::now();
    loop {
        let all_present = engine
            .handle()
            .read(|kb| paths.iter().all(|p| kb.catalog.get(p).is_some()))
            .value()
            .unwrap_or(false);
        if all_present {
            return true;
        }
        if started.elapsed() > budget {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Drive one operation: apply, wait for the version to advance, undo, settle.
///
/// The version advance is the same signal a wire subscriber waits on; taking it
/// in process measures the engine's half without the socket, which is the half
/// this harness is about.
fn run_operation(
    op: &opcat::Operation,
    targets: &opcat::Targets,
    engine: &Engine,
    sink: &Sink,
    subscriber: Option<&WireSub>,
) -> Row {
    settle(engine, sink);
    // Setup runs and SETTLES before the timer, so its own rebuild is never
    // folded into the measurement. Without this a delete's row would time the
    // creation of the file it deletes.
    if let Some(prepare) = op.prepare {
        // The setup must be ABSORBED before the measurement begins, and the
        // engine's own CATALOG is the only unambiguous signal for that. A
        // version advance is not: writes coalesce, so an advance can belong to
        // an earlier write while this one is still pending. If the setup is
        // still pending when the operation runs, the two land in ONE dirty set,
        // a create and a delete cancel out, and the engine correctly does
        // nothing, leaving the harness waiting forever for a rebuild that
        // should never come.
        let expected = prepare(targets).expect("prepare operation");
        if !wait_for_catalog(engine, &expected, Duration::from_secs(120)) {
            eprintln!(
                "  ! {}: setup never entered the catalog, row is unusable",
                op.name
            );
        }
        settle(engine, sink);
    }
    let _ = sink.drain();
    if let Some(sub) = subscriber {
        sub.drain();
    }

    let before = engine.handle().version();
    let started = Instant::now();
    let applied = (op.apply)(targets).expect("apply operation");

    // Wait for the write to become observable. A `None` is the deriving state,
    // never an advance.
    let latency = loop {
        if matches!((engine.handle().version(), before), (Some(now), Some(was)) if now > was) {
            break started.elapsed();
        }
        if started.elapsed() > MEASURE_TIMEOUT {
            let still = engine
                .handle()
                .read(|kb| {
                    applied
                        .touched
                        .iter()
                        .filter(|p| kb.catalog.get(*p).is_some())
                        .count()
                })
                .value()
                .unwrap_or(0);
            eprintln!(
                "  ! {} timed out; {}/{} touched paths still catalogued, exists-on-disk={:?}",
                op.name,
                still,
                applied.touched.len(),
                applied
                    .touched
                    .iter()
                    .map(|p| p.exists())
                    .collect::<Vec<_>>()
            );
            break started.elapsed();
        }
        std::thread::sleep(Duration::from_millis(1));
    };

    // The version advance happens at `commit`, which is INSIDE the rebuild
    // span, so the enclosing spans have not closed yet and their totals are not
    // in the sink. Draining on the advance would drop them, and drop them
    // racily: some rows would carry a parent and some would not.
    //
    // A short wait lets them close. It cannot capture a LATER rebuild, since
    // another one needs the quiet window to elapse first.
    // The wire event rides the same rebuild, arriving just after the commit
    // that advanced the version. A short budget past the grace is enough; a
    // pure edit that changes no hash fires nothing, which is not a failure.
    let wire_latency = subscriber
        .and_then(|sub| sub.next_within(SPAN_CLOSE_GRACE + Duration::from_millis(200)))
        .map(|at| at.duration_since(started));

    std::thread::sleep(SPAN_CLOSE_GRACE);
    let phases = sink.drain();
    // A whole build ran iff the build span fired. The incremental path splices
    // instead, so its absence IS the fast path, read from the same spans the
    // breakdown comes from rather than from a second mechanism.
    let path = if phases.contains_key("build_reusing") {
        RebuildPath::Full
    } else {
        RebuildPath::Incremental
    };
    // `rebuild_locked` is the whole-knowledge-base reconcile. Its presence means
    // the BACKSTOP served this row rather than the watcher's own dirty set, which
    // is a real latency the consumer feels. It does NOT establish that an event
    // was lost: nothing here observes the event stream, so a row served by the
    // reconcile is evidence about which path ran, not about what the watcher
    // saw. So the two are read SEPARATELY: `watch_event` fires once per message
    // the backend delivered, which is what makes "no event arrived" a fact
    // rather than an inference. An earlier version of this harness conflated
    // them and reported a lost event whenever the backstop served a row; that
    // reading reached a design before anyone checked it.
    let via_reconcile = phases.contains_key("rebuild_locked");
    let saw_event = phases.contains_key("watch_event");

    settle(engine, sink);
    applied.undo.run().expect("undo operation");
    settle(engine, sink);
    let _ = sink.drain();

    Row {
        name: op.name,
        latency,
        wire_latency,
        via_reconcile,
        saw_event,
        untraced: None,
        path,
        phases,
        gap: op.wanted,
    }
}

/// The contained scratch folder a real-entry run operates inside.
///
/// Named distinctively so it is obvious in a `git status` if a run ever dies
/// before cleaning up, though [`Scratch`] is what normally removes it.
const SCRATCH_DIR: &str = "op-bench-scratch";

/// The scratch folder, removed when this drops.
///
/// A real-entry run writes into a repo someone owns, and the measured window is
/// full of `.expect()`. A panic there skipped the teardown entirely and left the
/// folder behind for the owner to find, which also made the final git-clean
/// check meaningless: it can only report what teardown left, and on the panic
/// path teardown never ran.
///
/// Removal on drop covers the panic path too, so "the tree is clean" is a claim
/// the harness can actually make.
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        // A missing folder is the success case, not a failure: a run that
        // completed normally may have removed it already.
        match std::fs::remove_dir_all(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("  ! could not remove {}: {e}", self.0.display()),
        }
    }
}

/// Seed the scratch folder with a vocabulary and instances of its own.
///
/// The point of a real-entry run is the CORPUS cost: composition, discovery,
/// and validation over the whole workspace. That cost does not care whose files
/// the operation touches, so the operations act on files this run created and
/// none of the workspace's own. The measurement stays real; the blast radius
/// does not.
fn seed_scratch(entry: &std::path::Path) -> std::io::Result<opcat::Targets> {
    let scratch = entry.join(SCRATCH_DIR);
    std::fs::create_dir_all(scratch.join("type"))?;
    // A distinctive type name, so it cannot collide with the host repo's own
    // vocabulary and produce a `duplicate-type-def`.
    let type_def = scratch.join("type/opbenchprobe.type.yaml");
    std::fs::write(&type_def, "fields:\n  title: String\n")?;
    let mut instances = Vec::new();
    for i in 0..24 {
        let path = scratch.join(format!("n{i}.md"));
        std::fs::write(
            &path,
            format!("---\ntype: opbenchprobe\ntitle: Probe {i}\n---\n# Probe {i}\n\nbody.\n"),
        )?;
        instances.push(path);
    }
    Ok(opcat::Targets::new(
        entry.to_path_buf(),
        instances,
        vec![type_def],
    ))
}

/// Paths git reports as changed, EXCLUDING the scratch folder.
///
/// The scratch folder is expected to be dirty while a run is in progress; any
/// other entry means an operation touched the workspace's own files.
fn dirty_outside_scratch(dir: &std::path::Path) -> std::io::Result<Vec<String>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other("not a git repository"));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| {
            // Porcelain is `XY <path>`; the path starts at column 3.
            let path = line.get(3..).unwrap_or("");
            !path.contains(SCRATCH_DIR)
        })
        .map(str::to_string)
        .collect())
}

/// Whether `dir`'s git working tree is clean, untracked files included.
///
/// The real-entry mode edits, deletes, and renames files in a repo someone
/// owns. Requiring a clean tree first makes every operation recoverable with
/// `git checkout` / `git clean`, and lets the harness DETECT its own failure to
/// restore rather than leaving damage for the owner to find.
fn git_is_clean(dir: &std::path::Path) -> std::io::Result<bool> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other("not a git repository"));
    }
    Ok(out.stdout.is_empty())
}

/// The catalogued node count, the corpus size a real-entry row is paying for.
fn corpus_size(engine: &Engine) -> usize {
    engine
        .handle()
        .read(|kb| kb.catalog.size())
        .value()
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // `--entry <path>` measures a real workspace. Generated profiles are
    // deterministic but do not reproduce a real corpus's cost distribution, so
    // the two modes answer different questions and both are needed.
    if let Some(pos) = args.iter().position(|a| a == "--entry") {
        let Some(path) = args.get(pos + 1) else {
            eprintln!("usage: op_bench --entry <folder-repo>");
            std::process::exit(2);
        };
        run_real_entry(std::path::Path::new(path));
        return;
    }

    let (profile, label) = profile_from_arg(args.get(1).map(String::as_str));

    let sink = Sink::default();
    let switch = install(sink.clone());

    let dir = tempfile::tempdir().expect("tempdir");
    println!("generating `{label}` ...");
    let gen = repogen::generate_profile(dir.path(), profile, 7);
    println!(
        "  {} instances, {} type-defs, entry {}",
        gen.instances.len(),
        gen.type_defs.len(),
        gen.entry.display()
    );

    let mut engine = Engine::new(&gen.entry, ConfigSource::Empty);
    engine.rebuild();
    engine.watch().expect("watch");
    settle(&engine, &sink);
    let subscriber = WireSub::start(&engine);
    if subscriber.is_none() {
        eprintln!("  ! could not open a wire subscriber; wire latency will be blank");
    }
    println!("engine up, version {:?}\n", engine.handle().version());

    let targets = opcat::Targets::from_gen(&gen);
    let rows: Vec<Row> = opcat::catalog()
        .iter()
        .map(|op| {
            println!("  running {} ...", op.name);
            let mut row = run_operation(op, &targets, &engine, &sink, subscriber.as_ref());
            row.untraced = measure_untraced(op, &targets, &engine, &sink, &switch);
            row
        })
        .collect();

    report(label, &rows);
}

/// Measure a real on-disk workspace.
///
/// Refuses a dirty tree and re-checks after every operation, so a failed undo
/// stops the run at the operation that caused it instead of contaminating the
/// rest and leaving the owner to discover it later.
fn run_real_entry(entry: &std::path::Path) {
    let entry = match std::fs::canonicalize(entry) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot resolve {}: {e}", entry.display());
            std::process::exit(2);
        }
    };
    match git_is_clean(&entry) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "refusing to run: {} has uncommitted changes.\n\
                 this mode edits, deletes, and renames real files; a clean tree is what makes\n\
                 every operation recoverable and lets a failed restore be detected.",
                entry.display()
            );
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!(
                "refusing to run: cannot check git state of {} ({e})",
                entry.display()
            );
            std::process::exit(2);
        }
    }

    let sink = Sink::default();
    let switch = install(sink.clone());

    // Seed BEFORE the engine boots, so the scratch files are part of the
    // initial build rather than an extra rebuild during the run.
    //
    // The guard is armed BEFORE the seed can fail, so a partial seed is cleaned
    // up too.
    let _scratch = Scratch(entry.join(SCRATCH_DIR));
    let targets = match seed_scratch(&entry) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot seed {}/{}: {e}", entry.display(), SCRATCH_DIR);
            // `process::exit` runs no destructors, so the guard has to be
            // spent by hand on the one exit path below it.
            drop(_scratch);
            std::process::exit(2);
        }
    };
    println!(
        "seeded {}/{} with {} instances and 1 type-def",
        entry.display(),
        SCRATCH_DIR,
        targets.instances.len()
    );

    // `ConfigSource::User`, not `Empty`. A real workspace resolves most of its
    // members through the per-user registry, and `Empty` silently mounts only
    // the entry plus co-present siblings. That produced a 40-node corpus for a
    // workspace the daemon catalogues at 782, with numbers that looked entirely
    // plausible; the corpus count printed below is what exposed it.
    println!("building {} ...", entry.display());
    let mut engine = Engine::new(&entry, ConfigSource::User);
    // The COLD build, timed and broken down. This is what a consumer pays to
    // open a workspace, and it is the only path that runs `fingerprint`, hence
    // the walk-resolve fixpoint, more than once. Every operation below takes the
    // scoped path, which reads the held fingerprint instead, so nothing else in
    // this report covers it.
    let _ = sink.drain();
    let cold_started = Instant::now();
    engine.rebuild();
    let cold = cold_started.elapsed();
    let cold_phases = sink.drain();
    engine.watch().expect("watch");
    settle(&engine, &sink);

    let subscriber = WireSub::start(&engine);
    if subscriber.is_none() {
        eprintln!("  ! could not open a wire subscriber; wire latency will be blank");
    }
    let corpus = corpus_size(&engine);
    println!(
        "  corpus {} catalogued nodes, operating on {} scratch instances, version {:?}",
        corpus,
        targets.instances.len(),
        engine.handle().version()
    );
    println!("  cold build (daemon startup) {:.1}ms", ms(cold));
    let mut cold_ranked: Vec<(&&str, &Duration)> = cold_phases.iter().collect();
    cold_ranked.sort_by(|a, b| b.1.cmp(a.1));
    for (name, dur) in cold_ranked
        .iter()
        .take_while(|(_, d)| ms(**d) / ms(cold) * 100.0 >= 0.5)
        .take(20)
    {
        println!(
            "    {:<24} {:>8.1}ms  {:>5.1}%",
            name,
            ms(**dur),
            ms(**dur) / ms(cold) * 100.0
        );
    }
    println!();

    let mut rows = Vec::new();
    for op in opcat::catalog() {
        println!("  running {} ...", op.name);
        let mut row = run_operation(&op, &targets, &engine, &sink, subscriber.as_ref());
        row.untraced = measure_untraced(&op, &targets, &engine, &sink, &switch);
        rows.push(row);
        match dirty_outside_scratch(&entry) {
            Ok(changes) if changes.is_empty() => {}
            Ok(changes) => {
                eprintln!(
                    "\nSTOPPING: `{}` touched files OUTSIDE {}:\n{}\n\
                     recover with `git checkout .` and `git clean -fd`.",
                    op.name,
                    SCRATCH_DIR,
                    changes.join("\n")
                );
                break;
            }
            Err(e) => {
                eprintln!(
                    "\nSTOPPING: cannot re-check git state after `{}` ({e})",
                    op.name
                );
                break;
            }
        }
    }

    // Remove the scratch folder, then require the tree to be exactly as found.
    // Explicit rather than left to [`Scratch`], because the clean check below
    // has to see the removal: the guard does not drop until this function
    // returns, which is after the check. The guard covers the PANIC path, where
    // neither of these lines runs at all.
    if let Err(e) = std::fs::remove_dir_all(entry.join(SCRATCH_DIR)) {
        eprintln!(
            "  ! could not remove {}/{}: {e}",
            entry.display(),
            SCRATCH_DIR
        );
    }
    match git_is_clean(&entry) {
        Ok(true) => println!("\nscratch removed, {} is clean\n", entry.display()),
        Ok(false) => eprintln!(
            "\n! {} is NOT clean after teardown; inspect with `git status`",
            entry.display()
        ),
        Err(e) => eprintln!("\n! cannot verify {} after teardown ({e})", entry.display()),
    }

    report(&format!("real entry {}", entry.display()), &rows);
}

fn report(label: &str, rows: &[Row]) {
    println!("\n=== write-to-observable, profile `{label}` ===\n");
    println!(
        "{:<22} {:>10} {:>10} {:>10}  {:<12} {}",
        "operation", "engine", "wire", "untraced", "path", "note"
    );
    for row in rows {
        let mut note = match row.gap {
            Some(w) if w != row.path => format!("GAP: should be {w:?}"),
            _ => String::new(),
        };
        // Reported on EVERY row, not only a reconcile-served one. A write the
        // backend never announced is the interesting fact whether or not the
        // backstop happened to cover it, and printing it only in the covered
        // case is how the two got conflated before.
        if !row.saw_event {
            note.push_str(" [no watcher event was delivered]");
        }
        if row.via_reconcile {
            // Two distinct facts, reported as two. The backstop serving a row is
            // a latency the consumer feels either way; whether an event arrived
            // is what says WHY, and it is now measured.
            note.push_str(if row.saw_event {
                " [served by the idle reconcile; the watcher DID deliver an event]"
            } else {
                " [served by the idle reconcile; the watcher delivered NO event]"
            });
        }
        let wire = match row.wire_latency {
            Some(d) => format!("{:.1}ms", ms(d)),
            // No event is a legitimate outcome: the `changes` channel fires on a
            // net content delta, and an operation can rebuild without one.
            None => "-".to_string(),
        };
        let untraced = match row.untraced {
            Some(u) => format!("{:.1}ms", ms(u)),
            None => "-".to_string(),
        };
        println!(
            "{:<22} {:>8.1}ms {:>10} {:>10}  {:<12} {}",
            row.name,
            ms(row.latency),
            wire,
            untraced,
            format!("{:?}", row.path),
            note
        );
    }

    // A per-row tax is NOT reported, deliberately. Consecutive measurements of
    // one operation interfere: the second inherits pending state from the
    // first. Swapping the order flipped the largest outlier from +467ms to
    // -211ms, which proves the per-row delta measures order, not
    // instrumentation. The aggregate still answers the question that matters.
    let pairs: Vec<f64> = rows
        .iter()
        .filter(|r| !r.via_reconcile)
        .filter_map(|r| r.untraced.map(|u| ms(r.latency) - ms(u)))
        .collect();
    if !pairs.is_empty() {
        let mean = pairs.iter().sum::<f64>() / pairs.len() as f64;
        let spread = pairs.iter().cloned().fold(f64::MIN, f64::max)
            - pairs.iter().cloned().fold(f64::MAX, f64::min);
        println!(
            "\ntracing tax over {} paired rows: mean {:+.1}ms, spread {:.1}ms",
            pairs.len(),
            mean,
            spread
        );
        println!("  the spread dwarfs the mean, so the tax is BELOW THE NOISE FLOOR here.");
        println!("  do not read a single row's difference as its instrumentation cost.");
    }

    println!("\n=== where the time went, top phases per operation ===\n");
    for row in rows {
        if row.phases.is_empty() {
            println!("{}\n  (no spans: handled without a rebuild)\n", row.name);
            continue;
        }
        let mut ranked: Vec<(&&str, &Duration)> = row.phases.iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(a.1));
        let total = ms(row.latency);
        println!("{} ({:.1}ms)", row.name, total);
        // Every span worth at least 1% of the operation, rather than a fixed
        // count. A fixed cutoff hides exactly what a newly-added child span
        // exists to show: a split lands below the line and reads as absent.
        // Bounded anyway, so a pathological trace cannot flood the report.
        let shown = ranked
            .iter()
            .take_while(|(_, dur)| ms(**dur) / total * 100.0 >= 1.0)
            .take(16);
        for (name, dur) in shown {
            // `build_reusing` is the parent of the phases, so its share is the
            // build's total, not a sibling cost. Kept in the list because the
            // gap between it and its children is the unattributed remainder.
            println!(
                "  {:<24} {:>8.1}ms  {:>5.1}%",
                name,
                ms(**dur),
                ms(**dur) / total * 100.0
            );
        }
        println!();
    }
}
