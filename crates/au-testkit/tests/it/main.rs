//! The au-testkit proptest invariants as one linked binary.
//!
//! Every former `tests/<name>.rs` is a submodule here, so the crate links
//! one test binary and compiles proptest once. Run one former file with
//! `cargo test -p au-testkit --test it <name>::`.

mod body_scanner_invariants;
mod body_typing_invariants;
mod candidate_invariants;
mod compound_shape_invariants;
mod enum_shape_invariants;
mod graph_load_invariants;
mod inline_value_invariants;
mod instance_validation_invariants;
mod meta_invariants;
mod mixin_and_prefix_invariants;
mod multi_leaf_invariants;
mod reference_and_list_invariants;
mod sealed_leaf_invariants;
