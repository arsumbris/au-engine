//! Library surface for the `au` binary. The binary is the per-repo daemon plus
//! its management commands; `main.rs` is a thin clap dispatcher over this.

#[cfg(unix)]
pub mod daemon;
