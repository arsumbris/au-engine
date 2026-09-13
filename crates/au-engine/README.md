# au-engine

The continuously-held analysis of a knowledge base.

The pure crates answer one question and return. This crate holds the analysis
in memory and keeps it current: walk the repo, parse every file, build the
type graph, validate every instance, index references, then serve reads.

## What it holds

- `build(repo) -> KnowledgeBase` — one whole-knowledge-base full build. The held IR:
  - a file catalog, each entry carrying its parse (`FileParse`) tagged with a
    content hash.
  - the type graph.
  - the resolved analysis per instance (effective shape, candidates).
  - the reverse reference index (`backlinks`).
  - the merged diagnostic stream.
- `parse_file(path, bytes) -> FileParse` — the single per-file parse entry,
  owned and source-free so it survives across rebuilds while the hash holds.

## The continuous engine

- `Engine` holds the IR and rebuilds on change. A rebuild computes a cheap
  content fingerprint first and no-ops when nothing changed; otherwise it does
  a full build, swaps the held knowledge base, and advances a monotonic version.
- Lifecycle, scoped to one ref: Starting then Up, the ref Deriving then Ready.
  Reads return a `Read<T>` stamped with the observed version, or not-ready while
  Deriving.
- `Engine::watch` runs a file watcher that debounces disk changes to a
  quiescence window and drives rebuilds.

## The serving surface (Unix)

- `serve(handle, socket)` exposes a Unix-domain-socket endpoint with
  length-prefixed JSON frames, async on a private tokio runtime. Every outbound
  frame is tagged by `type` and carries a schema version; a read's `response`
  carries the readiness and, when ready, the observed version.
- `socket_path(entry)` derives the per-entry endpoint
  (`~/.arsumbris/au-engine/run/<hash>.sock`, the FNV-1a of the absolute entry path), so
  the daemon and any consumer reach it from the entry path alone, off a deep
  knowledge base path that could overrun the socket-path limit.
- Reads: diagnostics (whole-knowledge-base, or scoped by path / path-prefix / severity /
  code, unknown args rejected); the type graph (`types`, and `type` by name);
  instances of a type by closure membership (`instances_of`) with frontmatter
  field values; one instance's resolved view (`instance`) with the full value
  layer — effective values with contribution provenance, section presence, and
  the body event stream; references (`references_out`, `references_in`,
  `resolve_target`, `resolve_block_id`); the knowledge base (`dir_entries`, `frontmatter`,
  `content`, `top_level_dirs`); and a ready probe. Reads serve the held
  analysis; `content` is the one read that returns source-form text from the
  working tree.
- Subscriptions: a `subscribe` opens a connection-tied channel delivering an
  ack, an initial value when the channel has one, then notification-only change
  events driven by rebuilds. The closed catalog: `lifecycle`, `types`,
  `type_graph`, `link_graph`, `files`, `changes`, and `diagnostics` (scoped by
  the same filters as the read). Each data channel diffs its own projection across rebuilds and
  carries the delta in its event's scope hint — changed paths, changed type
  names, files whose diagnostics changed. A consumer never polls; it subscribes
  and is woken on every relevant change.
- One control verb, `shutdown`, lets a foreground server be told to stop; it
  parks on `ServeHandle::wait_for_shutdown` until then.
- `WIRE.md` is the consumer-facing contract: transport, framing, the socket
  convention, the frame envelopes, the read and subscription catalogs, and the
  consumer port mapping. `tests/port_contracts.rs` and `tests/subscribe.rs` are
  the standing guards against drift; `examples/watch.py` is a reference external
  consumer of the subscription wire.

## The wire-DTO layer

- `wire` holds the serializable shapes for the engine's reads, rendered by both
  the CLI's `--introspect` output and the serving surface. `introspect_graph`
  and `introspect_kb_instances` / `introspect_kb_graph` project the held
  IR into the wire shapes; spans carry canonical byte offsets plus the derived
  `line_col` rendering for every file the build read.

au-core stays pure analysis. au-cli renders this crate's output. This crate owns
the orchestration, the held state, the wire shapes, and the serving surface.

## Scope

In-memory, single-ref. A rebuild reads and re-parses only the changed files and
reuses every other file's parse (keyed by content hash), so per-edit read and
parse cost is bounded by what changed. The changed set comes from the writer:
the mutation channel passes its exact target, the watcher passes its event
paths. A configurable idle reconcile (`AU_IDLE_RECONCILE`, default on) does a
whole-knowledge-base pass after the watcher goes quiet to catch silently-dropped events;
disable it where every write goes through the mutation channel. For an instance
edit, add, delete, or rename, the resolved analysis recomputes incrementally too:
validation, the reverse-dependency indices, drift, and the served diagnostic
stream update by delta, and the held analysis is shared structurally (persistent
ordered maps plus `Arc`), so the whole rebuild is O(changed), byte-identical to a
full build. A type-def or vocabulary edit still recomputes whole-knowledge-base, and any
unhandled dirty set falls back to it. No git-blob-fault, no multi-ref sharing.
Change events carry per-channel deltas (files, paths, type names) from cheap
projection diffs; field- and entity-granularity deltas stay deferred for
uncertain need. Windows named-pipe transport is not built yet.
