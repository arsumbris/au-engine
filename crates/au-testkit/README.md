# au-testkit

Property generators, builders, and adversarial fixtures for the type system.

## Surface

- `arb_type_name` / `arb_field_name` / `arb_enum_literal` / `arb_enum_shape` — proptest strategies bound to the spec regex.
- `empty_type_def` / `type_def_with_parents` / `type_def_with_fields` / `type_def_with_enum_field` / `type_def_with_reference_field` / `type_def_with_list_field` / `type_def_sealed` — `TypeDef` builders.
- `instance_bare(claim, fields)` / `instance_list(claims, fields)` / `instance_field(key, value)` / `instance_field_seq(key, elements)` — `Instance` and `InstanceField` builders.
- `validate_simple(graph, instance)` / `validate_with(graph, idx, claims, instance)` — test-side wrappers around `au_core::validate` for callers that don't yet build a full `RepoIndex` / claims map.
- `cycle_of_size(n)` / `self_cycle(name)` — adversarial fixtures for cycle-detection tests.
- `repogen` — seeded deterministic cross-repo workspace generator (`generate` / `generate_profile`, `Knobs`, `Profile`), byte-identical per `(knobs, seed)`, for the scale benchmarks and the correctness fuzzer.
- `opcat` — the operation catalog, named repeatable write operations over a workspace, shared by the timing harness and the gate's rebuild-path assertions.

Integration tests live in one binary, `tests/it`, as modules:
- `graph_load_invariants` — graph-load determinism, cycle-detection termination, sealed-reachability, redeclare detection.
- `instance_validation_invariants` — required-field detection, unknown-claim detection, validator determinism under field permutation, idempotence.
- `enum_shape_invariants` — enum membership, parser round-trip, token-equality (order-sensitive), permutation-determinism.
- `reference_and_list_invariants` — refs (existing-typed / absent / wrong-type / `file*` existence-only), list elementwise validation, knowledge base-index resolution determinism.
- `mixin_and_prefix_invariants` — mixin commutativity, auto-unify equivalence, non-identical collision, prefix vs bare equivalence, per-originator partial-fill, mixed-bare-and-prefixed determinism, redundant-claim warnings.
- `body_scanner_invariants` / `body_typing_invariants` — markdown body-event scanning and body-slot typing.
- `candidate_invariants` — implicit-type candidate-scan invariants.
- `compound_shape_invariants` — compound shape-expression validation.
- `inline_value_invariants` — inline value-container validation.
- `meta_invariants` — meta-position and required-subtype-meta invariants.
- `multi_leaf_invariants` / `sealed_leaf_invariants` — multi-leaf and sealed-family closure behavior.
