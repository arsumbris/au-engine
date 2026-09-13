# Ars Umbris Engine

Rust workspace implementing the Ars Umbris engine — a typed, queryable substrate over a folder of markdown files with YAML frontmatter and `[[wiki-links]]`.

See [[moc - type system::au-type-system]] for the type-system spec (in the sibling repo `au-type-system`) and [[spec - diagnostic codes::au-type-system]] for the canonical enumeration of every diagnostic code the engine emits. `CHANGELOG.md` tracks notable engine and wire-contract changes for consumer repos.

## Crates

| Crate | Role |
|---|---|
| `au-cli` | the `au` binary: `au daemon start/stop/status` runs the per-repo daemon. Consumers speak the wire themselves (`crates/au-engine/WIRE.md`) |
| `au-engine` | the continuously-held analysis: `build` produces the held IR (catalog with per-file parse, type graph, resolved instances, backlink index, diagnostics); `parse_file` is the single per-file parse entry; `Engine` watches the knowledge base and rebuilds on change with a monotonic version; the `wire` module holds the serializable read shapes; a Unix-socket serving surface answers framed JSON reads and live subscriptions at `<repo>/.arsumbris/daemon.sock` |
| `au-core` | type-def + instance ASTs, type-graph, load-time checks, closure walks, validator, body-typing v5 (`body` template AST, `provenance` ValueContainer model, `body_validate` per-instance checks) |
| `au-parser` | filesystem port, frontmatter splitter, YAML AST with byte-offset spans, file-kind classifier, markdown body scanner (`scan_body` emitting Heading / FencedBlock / InlineCode / Wikilink / BlockIdMarker events) |
| `au-grammar` | slot-expression mini-language → `Shape` AST; primitives, inline closed enums, bare-name records (`rationale`), typed refs (`name*`), inline-or-reference (`name&`), list suffix (`[]` / non-empty `[+]`), and compound expressions (`<X \| Y>`, `<X & Y>`, with `*`/`&`/`[]`/`[+]` suffixes) recognized today |
| `au-diagnostics` | structured records, stable kebab-case codes |
| `au-references` | wikilink parser (with body-typing `:field` fragment), repo file index (`RepoIndex`), `[[target]]` resolution, cross-file `^block-id` block resolver (`resolve_block_id`) |
| `au-testkit` | property generators, builders, adversarial fixtures |

## Build

```sh
cargo build
cargo test -p <crate>
```

Verify at the smallest sufficient scope.
The full gate is `.claude/scripts/run-tests.sh`.
See `CLAUDE.md` for the cargo-iteration discipline.

### Recommended: faster linker

Apple's default `ld` is single-threaded; the workspace links many small binaries (each integration test is its own binary), so cold rebuilds spend a meaningful fraction of their time in the linker. The repo ships a `.cargo/config.toml` that tells cargo to use LLVM's `lld` — install it once and cold rebuilds drop ~20-40%.

```sh
# macOS
brew install llvm lld

# Linux
apt install lld   # Debian/Ubuntu
pacman -S lld     # Arch
```

The `.cargo/config.toml` covers `aarch64-apple-darwin`, `x86_64-apple-darwin`, and the two Linux-gnu triples. If your platform isn't covered, copy one of the entries and adjust the target triple — `-C link-arg=-fuse-ld=lld` is the only flag that matters; clang resolves the right backend per target.

## The daemon

```sh
au daemon start <repo>     # boot a daemon, serve in the foreground until stopped
au daemon status <repo>    # report whether a daemon is serving the knowledge base
au daemon stop <repo>      # ask the daemon to shut down
```

Consumers start a daemon and speak the wire over the Unix socket at `<repo>/.arsumbris/daemon.sock`. The contract is `crates/au-engine/WIRE.md`: framed JSON, a closed read catalog (diagnostics, the type graph, per-instance introspection, the candidate scan, references, the knowledge base), and live subscriptions. `examples/watch.py` is a reference external consumer. Schema is additive under `schema_version: 29`; consumers ignore unknown fields.

## Status

The validator runs end-to-end over the daemon. It checks:

- Single-claim AND multi-claim mixin instances (`type: [a, b]`) — auto-unify on token-equal field shapes; non-token-equal collisions surface as `mixin-collision`.
- colon-prefix field keys (`T:F`) — resolves the originator via `closure_of(T)`, validates the value against the originator's shape, supports per-originator required-field semantics.
- Redundant-claim warnings: `duplicate-claim` and `subsumption-in-mixin` (symmetric to slot-union subsumption rule), fired at both load time (type-def parents) and validate time (instance claims).
- Primitive shapes (`String / Number / Boolean / Date / DateTime / Url`), inline closed enums (`[v1, v2, ...]`), typed references (`name*`, including the built-in `file*` for any-repo-file resolution), and list shapes (`X[]` plus the non-empty variant `X[+]`, with elementwise validation through nested lists).
- Sealed-family per-claim exhaustiveness — claiming a sealed parent (or sealed intermediate in a nested sum) directly fires `sealed-parent-claimed`; the rule is per-claim, so a non-sealed sibling in a mixin does not excuse a sealed claim.
- Multi-leaf-in-sealed-family — an instance's `type:` claim list may contain at most one non-sealed leaf per sealed family (a sealed family is a discriminated union). Fires `multi-leaf-in-sealed-family` at file-level and inline-value identity claims. Innermost-only suppression on nested sums when inner and outer bucket identical leaf sets.
- Compound slot expressions: `<X | Y>` (any-of), `<X & Y>` (all-of), and the suffix matrix (`*`/`&`/`[]`/`[+]`) on compounds. Slot-union and slot-intersection subsumption fire `subsumption-in-slot-union` / `subsumption-in-slot-intersection` at load time — covers both subtype-by-closure and cardinality-refinement cases (`<T[] | T[+]>`, `<T[] & T[+]>`).
- Inline values at record-typed slots — bare-name records (`body: rationale`), `&` (inline-or-reference) slots, and compound `&` slots all accept inline YAML maps; nested inline values fall out of the recursive dispatch.
- Inline-value `type:` disambiguation — four cases: optional at non-sealed record slots; required at sealed slots; required at union slots (identifies branch); required at intersection slots (must satisfy all branches via single name or mixin). New diagnostic codes `inline-value-missing-type` and `inline-value-type-not-compatible`.
- `<String | T*>` wikilink disambiguation — wikilink-pattern strings (`"[[...]]"`) route to the reference branch in primitive-vs-reference unions; non-wikilink strings stay on the String branch. Branch order is irrelevant.
- Meta sub-region body validation — each `meta:` block's body validates against its named meta-type-def's effective shape; required-field-absent, field-shape-mismatch, unknown-type-claim, and the universal sealed-leaf rule all fire at meta sites. Mixin form (`type: [a, b]`) inside `meta:` is rejected with `meta-mixin-not-supported`.
- `meta: []` suppression marker — distinguished from absent `meta:` at parse time so the consumer walk can stop on suppression while transparent absence falls through to ancestors.
- `lookup_meta(graph, host, meta_type)` canonical walk helper — public API in `au-core::meta`. Walks host's `type:` chain ancestors, respects suppression, returns the first matching `MetaBlock`. Walks do not recurse into encountered meta-type-defs.
- The `types` read exposes each TypeDef's `meta_blocks` (type-name discriminator + body fields with values + per-block source span). Distinguishes the three states on the wire: `null` (absent), `[]` (suppression), `[..]` (declared).
- The graph and instance introspection also expose the body-typing v5 model: per type-def, source-form `body` + post-splice `effective_body` (every `use: T` resolved); per instance, `effective_values` (ValueContainers across all four surfaces — frontmatter / body wikilink / body yaml block / body inline code — with `kind`-tagged values: scalar / reference / inline_record), `section_presence` (top-level declared sections), and the raw `body_events` stream (heading / fenced_block / inline_code / wikilink-with-parsed-fragment-breakdown / block_id_marker). Pure-YAML instances emit `body_events: null`.
- Implicit identity candidates via the `candidates` read. Top-level (full required-set match against frontmatter regardless of closure/extras partition) and nested (recursive per-scope walk through inline values with RFC 6901 JSON-Pointer handles). case 1 unclaimed inline values resolve their implied identity from the slot's demanded type. Sealed parents filtered (unactionable); sibling-leaf candidates surface with a `supersedes` list naming the currently-claimed leaves they would replace (multi-leaf-in-sealed-family). Ranking. Advisory only — never blocks a knowledge base.

The build constructs a `RepoIndex` over every regular file under the repo root (instances, type-defs, assets) so reference resolution sees the full file set; case-collision diagnostics fire at index build time.
