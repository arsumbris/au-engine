//! The au-cli integration suite as one linked binary.
//!
//! Every former `tests/<name>.rs` is a submodule here, so a workspace edit
//! relinks one binary instead of one per file. Run one former file with
//! `cargo test -p au-cli --test it <name>::`.
//!
//! Note: `scenarios` owns the insta snapshots under `tests/it/snapshots/`. As a
//! submodule its `module_path!()` is `it::scenarios`, so the snapshot files are
//! prefixed `it__scenarios__` (insta derives the prefix from the module path).

mod daemon;
mod file_io_diagnostics;
mod scenarios;
