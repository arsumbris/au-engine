//! The held representation of a knowledge base.
//!
//! Where the pure crates answer one question and return, this crate holds the
//! analysis in memory: it walks the knowledge base, builds the type graph, parses and
//! validates every instance, and keeps the result so consumers can read it
//! many times without re-running the pipeline.
//!
//! au-core stays pure analysis (graph, closure, effective-shape, validate).
//! au-cli renders. This crate owns the orchestration between them and the
//! state that orchestration produces.

mod backlinks;
mod build;
mod crossref;
mod crosstype;
pub mod diagnostics;
mod engine;
mod engine_schema;
mod ensure_mixin;
#[cfg(unix)]
mod gitwatch;
#[cfg(unix)]
mod gitwriter;
mod incremental;
#[cfg(unix)]
mod inline;
mod ir;
#[cfg(unix)]
mod mutate;
mod neighborhood;
mod overlay;
mod parse;
mod pathset;
#[cfg(unix)]
mod pinned;
#[cfg(unix)]
mod pkgcache;
#[cfg(unix)]
mod promote;
mod readme;
mod refnames;
#[cfg(unix)]
mod rename;
pub mod repo;
mod resolution_build;
#[cfg(unix)]
mod serve;
#[cfg(test)]
mod spancap;
mod typerefs;
#[cfg(unix)]
mod value_validate;
pub mod wire;
mod yaml_render;

pub use backlinks::{Backlink, RefSurface};
pub use build::{build, build_reusing, verify_entry, BuildError};
pub use engine::{Engine, EngineHandle, EngineState, Read, ReconcilePolicy, RefState};
// The incremental fast path, exposed only so `examples/rebuild_bench.rs` can
// time the live recompute + clone-and-patch apply. Not part of the documented
// API: consumers drive rebuilds through the engine, never these directly.
#[doc(hidden)]
pub use incremental::{apply_recompute, recompute_dirty, InstanceRecompute};
pub use ir::{
    BuildOutcome, ContentHash, DiagStream, FileEntry, KnowledgeBase, OrdMap, ParseLayer,
    RepoGraphs, RepoIndexes, ResolutionGraphs, ResolvedInstance,
};
pub use parse::{parse_file, FileParse};
pub use repo::{
    device_root, discover_repos, ConfigSource, HomeUnset, RegistryLocation, Repo, RepoEntry,
    RepoMap, RepoName, UserRegistry, Workspace,
};
#[cfg(unix)]
pub use serve::{
    recover_crashed_saga, serve, socket_file_name, socket_path, Client, SagaFailure, SagaRecovery,
    SagaRecoveryReport, ServeHandle, SCHEMA_VERSION,
};
