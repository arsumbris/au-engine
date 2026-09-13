//! Capture the span names emitted while a closure runs, deterministically.
//!
//! Test-only. It exists so that "this seam is still instrumented" can be
//! ASSERTED, after a `#[tracing::instrument]` attribute silently rebound to a
//! function inserted beneath it and a span vanished from every trace with
//! nothing failing.
//!
//! # Why this is not just `with_default`
//!
//! `tracing` caches callsite interest GLOBALLY, once, keyed on the callsite. A
//! thread-local subscriber therefore races the cache: if a callsite is first hit
//! on a thread that has no subscriber, it caches as never-interested, and a
//! later capture on another thread never sees it. Under `cargo test`'s parallel
//! threads that is a genuine coin flip, and it was observed flaking roughly one
//! run in six. `rebuild_interest_cache` narrows the window without closing it,
//! because another thread can hit the callsite in between.
//!
//! So the subscriber is installed ONCE, GLOBALLY, for the whole test binary, and
//! stays installed. Interest is then stable for every callsite, and capturing is
//! a matter of switching a sink on and off rather than swapping subscribers.
//!
//! A flaky assertion is worse than an absent one: it gets muted, and then the
//! thing it guarded regresses unobserved. That is precisely the failure this
//! module exists to prevent, so it must not reproduce it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread::ThreadId;

use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

/// The capturing thread and its sink, while a capture is active.
///
/// Keyed by THREAD, not merely on or off. The subscriber is global, so it sees
/// every span the whole test binary emits; recording all of them would fold
/// concurrent tests' spans into the capture and make an assertion like "no whole
/// build ran" fail because some unrelated test built one. Scoping to the
/// capturing thread is what makes the result about the closure rather than about
/// the machine's scheduling.
static SINK: Mutex<Option<(ThreadId, Vec<String>)>> = Mutex::new(None);

/// Whether any capture is in progress at all.
///
/// The subscriber is installed for the REST OF THE BINARY once any test
/// captures, so `on_new_span` then runs for every span every later test emits,
/// on every thread. Most of those tests are not capturing, and taking a process
/// wide mutex to discover that is the common case paying for the rare one.
///
/// A relaxed load is enough: the flag only decides whether to look, and the sink
/// itself is still behind the mutex, still thread-keyed, and still the thing
/// that decides whether to record. A stale `false` loses a span from a capture
/// this thread is not running.
static ANY_CAPTURE: AtomicBool = AtomicBool::new(false);

/// Serializes captures against each other, so two tests cannot contend for the
/// single sink slot. Held for the whole of [`capture`].
static CAPTURING: Mutex<()> = Mutex::new(());

struct Recorder;

impl<S: tracing::Subscriber> Layer<S> for Recorder {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::Id,
        _ctx: Context<'_, S>,
    ) {
        // The fast path out, before the mutex. Everything below still holds:
        // the sink is thread-keyed, and REMOVING that key is not an option. A
        // global sink was tried and broke immediately, because a concurrent
        // test building a workspace put a `build_reusing` span into a capture
        // asserting none had run.
        if !ANY_CAPTURE.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut sink) = SINK.lock() {
            if let Some((owner, names)) = sink.as_mut() {
                if *owner == std::thread::current().id() {
                    names.push(attrs.metadata().name().to_string());
                }
            }
        }
    }
}

/// Install the global subscriber, once.
fn install() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // Ignore the error: another test harness in this process may have set
        // one first, in which case capture simply records nothing and the
        // assertions that depend on it fail loudly rather than silently.
        let _ = tracing_subscriber::registry().with(Recorder).try_init();
    });
}

/// The span names `f` emits ON THE CALLING THREAD, in creation order.
///
/// Work `f` hands to another thread is NOT captured; every current caller drives
/// the engine synchronously, and the alternative folds in whatever else the test
/// binary is doing at the time. Captures are serialized, so concurrent callers
/// queue rather than interleave.
pub(crate) fn capture(f: impl FnOnce()) -> Vec<String> {
    install();
    let _one_at_a_time: MutexGuard<'_, ()> = CAPTURING.lock().unwrap_or_else(|e| e.into_inner());
    *SINK.lock().unwrap() = Some((std::thread::current().id(), Vec::new()));
    // Ordered after the sink is in place, and cleared before it is taken, so the
    // flag is never true with nothing behind it.
    ANY_CAPTURE.store(true, Ordering::Relaxed);
    f();
    ANY_CAPTURE.store(false, Ordering::Relaxed);
    SINK.lock()
        .unwrap()
        .take()
        .map(|(_, names)| names)
        .unwrap_or_default()
}
