//! The continuous engine: lifecycle, the rebuild loop, and the live watcher.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use au_engine::{ConfigSource, Engine, EngineState, Read, RefState};

/// A knowledge base with a `note` type whose `link` field is an optional file
/// reference. Returns the canonicalized root (macOS tempdirs are symlinks, so
/// canonicalizing keeps walked and resolved paths in one form).
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    // Build in a `v` subdir so the folder basename matches the seeded repo name
    // (a bare tempdir is named `.tmpXXXX`, which would drift on the name).
    let root = fs::canonicalize(dir.path()).unwrap().join("v");
    fs::create_dir_all(&root).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  link?: file*\n",
    )
    .unwrap();
    fs::write(root.join("a.md"), "---\ntype: note\n---\n").unwrap();
    (dir, root)
}

fn poll_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    cond()
}

#[test]
fn deriving_until_first_build_then_ready() {
    let (_dir, root) = fixture();
    let engine = Engine::new(&root, ConfigSource::Empty);

    assert_eq!(
        engine.lifecycle(),
        (EngineState::Starting, RefState::Deriving)
    );
    assert_eq!(engine.version(), None);
    assert_eq!(engine.read_diagnostics(), Read::NotReady);

    engine.rebuild();

    assert_eq!(engine.lifecycle(), (EngineState::Up, RefState::Ready));
    assert_eq!(engine.version(), Some(1));
    assert!(matches!(
        engine.read_diagnostics(),
        Read::Ready { version: 1, .. }
    ));
}

#[test]
fn edit_advances_version_and_updates_diagnostics() {
    let (_dir, root) = fixture();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();

    // Clean knowledge base: ready at version 1, no diagnostics.
    let Read::Ready { version, value } = engine.read_diagnostics() else {
        panic!("expected ready");
    };
    assert_eq!(version, 1);
    assert!(
        value.is_empty(),
        "clean knowledge base has no diagnostics, got {value:?}"
    );

    // Introduce a dangling reference.
    fs::write(
        root.join("a.md"),
        "---\ntype: note\nlink: \"[[missing]]\"\n---\n",
    )
    .unwrap();
    engine.rebuild();

    let Read::Ready { version, value } = engine.read_diagnostics() else {
        panic!("expected ready");
    };
    assert_eq!(version, 2, "edit advanced the version");
    assert!(
        value
            .iter()
            .any(|d| d.code.as_str() == "reference-target-missing"),
        "dangling reference is diagnosed, got {value:?}"
    );

    // A touch with identical bytes: same fingerprint, no rebuild, no advance.
    fs::write(
        root.join("a.md"),
        "---\ntype: note\nlink: \"[[missing]]\"\n---\n",
    )
    .unwrap();
    engine.rebuild();
    assert_eq!(
        engine.version(),
        Some(2),
        "an unchanged-content touch does not advance the version"
    );
}

#[test]
fn version_receiver_wakes_on_every_advance() {
    let (_dir, root) = fixture();
    let engine = Engine::new(&root, ConfigSource::Empty);
    let handle = engine.handle();

    // A receiver subscribed before any build sees version 0; the version read
    // is absent while Deriving.
    let mut rx = handle.subscribe_version();
    assert_eq!(*rx.borrow(), 0, "Deriving broadcasts version 0");
    assert!(handle.version().is_none(), "no version while Deriving");

    // First build: the version advances to 1 and the receiver observes it.
    engine.rebuild();
    assert!(
        rx.has_changed().unwrap(),
        "the version receiver was notified"
    );
    assert_eq!(*rx.borrow_and_update(), 1);
    assert_eq!(handle.version(), Some(1));

    // An edit advances the version and wakes the receiver again.
    fs::write(
        root.join("a.md"),
        "---\ntype: note\nlink: \"[[missing]]\"\n---\n",
    )
    .unwrap();
    engine.rebuild();
    assert!(rx.has_changed().unwrap());
    assert_eq!(*rx.borrow_and_update(), 2);
    assert_eq!(handle.version(), Some(2));
}

#[test]
fn read_is_coherent_at_one_version() {
    let (_dir, root) = fixture();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();

    // The catalog and diagnostics read back at the same version.
    let catalog_files = engine.read(|v| v.catalog.size());
    let diags = engine.read_diagnostics();
    assert!(matches!(catalog_files, Read::Ready { .. }));
    assert!(matches!(diags, Read::Ready { version: 1, .. }));
}

#[test]
fn watcher_rebuilds_on_disk_change() {
    let (_dir, root) = fixture();
    let mut engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();
    assert_eq!(engine.version(), Some(1));

    engine.watch().expect("watch");
    // Let the watcher settle before mutating.
    std::thread::sleep(Duration::from_millis(100));

    write_dangling_link(&root);

    let advanced = poll_until(
        || engine.version().unwrap_or(0) > 1,
        Duration::from_secs(10),
    );
    assert!(advanced, "watcher rebuilt after the disk change");

    let diags = engine.read_diagnostics().value().unwrap();
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_str() == "reference-target-missing"),
        "watcher-driven rebuild picked up the new diagnostic, got {diags:?}"
    );
}

fn write_dangling_link(root: &Path) {
    fs::write(
        root.join("a.md"),
        "---\ntype: note\nlink: \"[[missing]]\"\n---\n",
    )
    .unwrap();
}
