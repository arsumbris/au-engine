# The daemon wire

The contract a consumer binds to.
One daemon serves one entry point over a Unix socket.

The engine is the source of truth for this shape.
Consumers normalize names to their own conventions.
(Snake_case to camelCase, enum values, the diagnostic mapping below.)


## Transport

Unix domain socket, one endpoint per entry point.

Socket path:
- `~/.arsumbris/au-engine/run/<hash>.sock`, outside the repo.
- derived by `au_engine::socket_path(entry)`.
- `entry` is the absolute path the engine was pointed at: a folder-repo
  DIRECTORY (a repo carrying `.arsumbris/repo.yaml`, composed by its optional
  `.arsumbris/workspace.yaml`). A non-repo entry is refused before serving. Each
  entry folder gets its own socket, so sibling workspace folders never collide.
- `<hash>` is the FNV-1a-64 of the entry path's bytes, rendered as 16 lowercase
  hex digits. A consumer derives the same path by canonicalizing (realpath) the
  entry it was given, then hashing its bytes:
  - `hash = 0xcbf29ce484222325`; for each byte `b`: `hash = (hash ^ b) * 0x100000001b3` (wrapping u64).
  - path: `${HOME}/.arsumbris/au-engine/run/${hash:016x}.sock`.
- the socket lives outside the repo so a deep repo path can no longer overrun
  the platform `sun_path` limit (the SUN_LEN failure).
- both sides must canonicalize the entry first, so they hash an identical string.
- the wire is UNAUTHENTICATED; the trust boundary is the owning user. The socket
  path is world-derivable (a non-secret hash of a non-secret entry path), so
  protection is file-mode: the socket is `0o600` and its `~/.arsumbris/au-engine/run`
  directory `0o700`, both owner-only. A multi-user deployment would need real
  caller authentication, not just file mode.

Framing:
- each message is a 4-byte big-endian length prefix, then that many bytes of JSON.
- the length prefix has a maximum of 16 MiB; a larger prefix is a protocol violation and the daemon closes the connection without reading the body.
- a `read` is one request frame, one response frame.
- a `subscribe` is one request frame, then many frames over time: an ack, an optional initial value, then change events until the connection closes.
- every outbound frame is tagged by a `type`, so a consumer demultiplexes responses and events on the one connection.
- the connection stays open across many round trips.

Windows named-pipe transport is deferred.
The framing carries over unchanged.


## Content hash

A file's content hash is FNV-1a 64-bit over its raw bytes, the same algorithm as the socket-path hash above. It is a DECLARED SDK contract, not an internal detail.

- `hash = 0xcbf29ce484222325`; for each byte `b`: `hash = (hash ^ b) * 0x100000001b3` (wrapping u64). Rendered as 16 lowercase hex digits where it crosses the wire (e.g. the `content` read's `hash`).
- a consumer holding a file's bytes computes the identical hash LOCALLY, with no engine round-trip, to arm read-before-write CAS. The engine's write floor still compares its own authoritative hash at write time, so correctness is unchanged whether the consumer read through the engine or not.
- the algorithm MAY change (e.g. to a git blob oid once the engine faults blobs by oid), but only as a COORDINATED change: `crates/au-engine/content-hash-vectors.json` and the au-engine-sdk mirror update together, never silently.
- `content-hash-vectors.json` is the published test vector, a set of `input` byte-strings mapped to their expected hash. The engine's `content_hash_vector_tests` pins it, so an uncoordinated algorithm change breaks the engine build rather than drifting a mirror silently. A TS mirror verifies itself against the same file.
- additive, no `schema_version` bump: blessing an existing algorithm as a contract adds a commitment and a vector, it changes no wire field.


## Request

A JSON object naming a read, a subscription, or a mutation.

A read names a capability under the `read` key, arguments are sibling keys:

```json
{ "read": "resolved", "path": "content/a.md" }
```

A subscription names a channel under the `subscribe` key, arguments are sibling keys, symmetric with a read:

```json
{ "subscribe": "diagnostics", "path": "content/a.md" }
```

A mutation names a primitive under the `mutate` key, arguments are sibling keys:

```json
{ "mutate": "write_file", "path": "content/a.md", "content": "..." }
```

The sibling verbs (`read`, `subscribe`, `mutate`) are tagged unions: the key's value names the variant (`"write_file"`, `"resolved"`, ...) and the arguments sit beside it.

Path arguments:
- absolute, or relative to the repo root.
- catalog keys are absolute, so either form lands on the same file.


## Frames

Every outbound frame is a JSON object tagged by `type`, carrying `schema_version`.

- `schema_version`
  - the wire version, an integer, currently `29`.
  - present on every frame.
  - v17 normalizes EVERY read result to the uniform envelope (see "The result
    envelope" below): each read's `result` is an object carrying its payload
    under the read's own name, so no read returns a bare array or scalar. The
    same bump renames five verbs (`list_imports` → `imports`, `resolved` →
    `instance`, `children` → `dir_entries`, `ready` → `lifecycle`, `backlinks` →
    `references_in`), splits `type`'s batch selector into `type_batch`, renames
    `content`'s inner `content` field to `text`, rescopes `top_level_graphs`
    into a workspace-wide repo-tagged `top_level_dirs`, scopes `overview` by
    `repo` / `scope` with `scope` defaulting to `own`, adds those filters to
    `diagnostics` / `diagnostic_counts`, and promotes `hubs` to a read.
  - v16 reshapes the `members` / `resolve_member` member shape, dropping the
    `primary` boolean and adding `local` (a live working tree vs a read-only
    cache snapshot, the LOCATION axis) and `role` (`entry` / `edit` / `discover`
    / `dep`); `editable` is now the ROLE axis (an editable authoring surface),
    no longer location-derived.
  - v15 reshapes the hardwired `au.engine.workspace` def, a single `primary`
    selection becomes two optional lists `edit?` / `discover?`, and adds
    `au.engine.workspace-lock` (the discover-closure pin, parallel to
    `au.engine.repo-lock`); both surface on `type_system_reference` and
    `instances_of`.

The frame types:
- `response` — the reply to a `read`.
- `ack` — the reply to a `subscribe`.
- `initial_value` — a channel's current state, delivered once after the ack.
- `change_event` — a notification that a subscribed channel changed.
- `resolved` — the reply to a `resolve` (see the resolve verb below).
- `error` — a request frame that names no known read or subscription, or fails to parse.

Schema evolution is additive.
- new fields, and new `type` values, may appear.
- consumers must ignore unknown fields, unknown `type` values, and unknown enum values.
- a removal, rename, or type change bumps `schema_version`.

### Request correlation

A request may carry an `id`, any JSON value the client picks to correlate the reply.
- the daemon echoes it on the request's direct reply: the `response` to a `read`, the `ack` to a `subscribe`, the response to a `mutate`, the `resolved` to a `resolve`, and the `error` for any of them.
- a client that sets a unique `id` per request settles replies by `id`, not by arrival order.
- the per-subscription frames after the ack (`initial_value`, `change_event`) carry `subscription_id`, not the request `id`; the client maps the two through the ack.
- `id` is optional. omitted in, omitted out. the daemon never validates or dedups it; the client owns id allocation.
- replies stay in request order, so a client that omits `id` can still settle by order.

### response

The reply to a `read`, a fixed envelope.

- `type`: `"response"`.
- `ready`
  - false while the ref is still Deriving, the first build not yet done.
  - reads resolve only when true.
- `version`
  - the ref version the result was observed at.
  - a `ready: true` response always carries a numeric `version`; a `ready: false` response always omits it.
  - so `ready` and `version` are coupled: the consumer may rely on `version` being present whenever `ready` is true.
  - monotonic within one connection, a cache-validity token.
- `result`
  - the read's value, present when ready.
  - one exception: the `lifecycle` probe carries its `result` even while `ready: false`. The version guarantee above still holds; only `result` is special.

A not-ready response carries `ready: false` and no `version`.
The consumer subscribes to `lifecycle` rather than polling, or retries.

### error

- `type`: `"error"`.
- `error`: a string, why the request frame could not be handled.
- `for`: the request shape the error answers, one of `read`, `subscribe`, `mutate`, `resolve`, `unknown`.
  - lets a client route the error to the right pending request even when it sent no `id`.
  - `unknown` when the bytes were not JSON, or named none of the verbs.
- `id`: the request's correlation id, echoed when it carried one. absent otherwise.

Set for a frame naming none of the verbs, naming an unknown read or subscription, or failing to parse.
A malformed `subscribe` is an `error` frame, not an `ack` with `accepted: false`; `for: "subscribe"` distinguishes it.


## Spans

Every span on the wire carries UTF-8 byte offsets into the file.
Ranges are half-open, start inclusive, end exclusive.
Byte offsets are the canonical position model.

Spans also carry `line_col`, the derived line/column rendering:

```json
{ "start": 74, "end": 109, "line_col": { "start": { "line": 6, "col": 1 }, "end": { "line": 7, "col": 1 } } }
```

- `line` and `col` are 1-based.
- `col` counts UTF-8 bytes from the line start, plus one.
  - deterministic regardless of encoding width.
  - a consumer needing character or UTF-16 columns converts within the one line.
- lines split at `\n` only, a `\r` counts into the column.

`line_col` is omitted for spans into files the build never read (assets, unreadable files).
It is always present for read files.


## The read catalog

Each read names its request arguments and its result shape.

### The result envelope

**Every read's `result` is an object carrying its payload under the read's own name.**

`result.<verb>` is always how a consumer reaches the payload.
- a list read wraps its array, `{ "read": "diagnostics" }` answers `{ "diagnostics": [...] }`.
- a nullable read wraps its null, `{ "read": "type" }` answers `{ "type": <def> }` or `{ "type": null }`, so "not found" is reached the same way as a hit.
- a record read wraps its record, `{ "read": "type_tree" }` answers `{ "type_tree": { "roots": [...], "nodes": [...] } }`.
- no read returns a bare array or a bare scalar.

The key is the verb, always, with no exceptions.
- a consumer derives it from the request it just sent, so no per-read table is needed.
- an SDK reads every response through one accessor, `result[verb]`.
- a new read inherits the rule; a generic test drives the whole catalog and asserts it, so a read cannot drift out of compliance silently.

Sibling keys beside the payload are envelope-level metadata ABOUT it, never part of it.
- `subtypes` answers `{ "base": "note", "subtypes": [...] }`, echoing the argument.
- `instances` answers `{ "count": 12, "aborted_at_load": false, "instances": [...] }`.
- a consumer reading `result[verb]` never needs them; they annotate.

### Scope defaults

Several reads take `scope: own | all`. The default is NOT uniform, and the split is one distinction, applied consistently.

**Actionability reads default to `own`**: `overview`, `top_level_dirs`, `hubs`, `diagnostics`, `diagnostic_counts`.
- they answer about content the user can ACT on, so a read-only dependency's contribution is noise in the default answer.
- a validation error inside a mounted dependency is not something the caller can fix; the workspace-health codes (`peer-unmounted`, `edit-member-unmounted`, `dependency-cache-miss`) anchor at the DECLARING repo's manifest, so they stay visible under `own` — nothing actionable is hidden.

**Vocabulary reads default to `all`**: `types`, `type_counts`, `subtypes`, `imports`.
- they answer what EXISTS to author against, which includes every peer type you can name.
- you write `book::library`, so hiding peer types by default would hide the thing you are using and make completion, hover, and the type tree look broken.

So `overview.type_counts` (own-scoped) and `type_counts({})` (all-scoped) deliberately disagree. That is the one gap, and it is why `overview` ECHOES its resolved `repo` / `scope`: a consumer drilling into a field passes the echoed args rather than assuming its own default matches.

**A LOCATION PIN changes the DEFAULT, never an explicit `scope`.**
- no pin: `scope` defaults to `own`.
- a pin — a named `repo`, or on `diagnostics` / `diagnostic_counts` a `path` or `path_prefix` — defaults `scope` to `all`, because the caller already narrowed to what they want. Defaulting to `own` there would answer an explicit request with an EMPTY list whenever the pinned member or file is a dependency (an editor consumer asking for the open file's diagnostics, when that file lives in a read-only dependency, must not silently see "no problems").
- naming a single file is at least as explicit a narrowing as naming a repo, so it carries the same guard.
- an explicit `scope` always wins and always composes (AND) as written, so `{repo: "base", scope: "own"}` or `{path: "<dep file>", scope: "own"}` legitimately yields nothing — the caller stated both filters.

Where a payload record would carry a field named after its own read, the FIELD is renamed, never the key.
- `content` serves `{ "content": { "text": "...", "hash": "...", "commit": "..." } }`, not `content.content`.
- renaming the key instead would break derivability catalog-wide to fix one field.

Absent or unresolved lookups return a null PAYLOAD, `{ "<verb>": null }`, not a bare `result: null` and not an error.

**diagnostics.**
- args: the diagnostics filters, all optional, all compose (AND). No args is whole-knowledge-base.
  - `path`: diagnostics in exactly this file.
  - `path_prefix`: diagnostics in files under this directory, matched on whole path components (`su` does not cover `sub/`).
  - `severity`: `error` | `drift` | `warning` | `hint`.
  - `code`: one exact diagnostic code.
  - `repo`: diagnostics owned by exactly this member. Absent spans every mounted member.
  - `scope`: `own` (default, or `all` when any location pin — `repo` / `path` / `path_prefix` — is present) / `all`. Orthogonal to `repo`, which selects WHICH member. See "Scope defaults" above.
  - `limit`: at most this many entries, after `offset`. Absent returns the whole filtered set.
  - `offset`: skip this many filtered entries before the page. Absent is 0.
- an unknown arg or an unknown severity value is an `error` frame, never silently ignored: a typo'd filter must not return the unfiltered set.
- result: `{ diagnostics: [...] }`, the diagnostics in scope, paged by `limit` / `offset`.
- each: `code`, `severity`, `message`, `span { file, range { start, end }, line_col }`, optional `related` (an array of spans), optional `fix { description }`.
- **`schema_version` 17**: `repo` / `scope` are new. Ownership is the member owning the diagnostic's `span.file`, resolved by the deepest-containing-repo rule, so a diagnostic inside a nested member attributes to that member and not to its container. A diagnostic whose file belongs to no mounted member is OUT of scope whenever `repo` or `scope: own` is engaged: an unowned diagnostic cannot answer "is this mine", and including it would let a scoped call return something the caller did not ask for. Forced by `overview`'s mirror property — `overview.diagnostic_counts` is own-scoped, so the read had to be able to express it.
- page order is the served stream's stable order: by source path, then by position within the source.
- no total rides the array; a consumer pages until a short page (`< limit`), or asks `diagnostic_counts` for the total. Paging is read-only: the `diagnostics` subscription channel streams the full set and ignores `limit` / `offset`.

**diagnostic_counts.**
- args: the same diagnostics filters as `diagnostics` (`path` / `path_prefix` / `severity` / `code`). `limit` / `offset` are accepted but ignored, counts are over the full filtered set.
- result: `{ total, by_severity, by_code }`, the shape of the problem without materializing every entry.
  - `total`: the filtered count.
  - `by_severity`: a map of severity string to count, only present severities, name-sorted.
  - `by_code`: a map of code to count, name-sorted.
- lets a consumer show "79 navigational-target-not-found" as one line instead of 79 entries, and is cheaper at the source than serializing the whole set to count it.

**types.**
- args, all optional, an unknown arg is an `error` frame (never a silently-unfiltered set), matching the diagnostics filters.
  - `repo`: absent, the workspace-wide read, every type-def across all members, deduped to its owner copy (the unmarked one, which carries meta), name-sorted. The enumeration "what type-defs exist across the mounted workspace." Present, scope resolution to one member's whole graph, by its declared name, the borrowed copies it resolves included. An unknown `repo` returns null, the wire's unresolved-lookup signal, distinct from a known repo with no types (an empty array).
  - `summary`: `true` projects each entry to a lightweight form (below) instead of the full def. Absent / `false` is full detail. The browse form for "what types exist"; the single `type` read is the detail-on-demand dual, keyed by the `name` / `hash` a summary entry carries.
  - `limit`: at most this many entries, after `offset`, over the name-sorted set. Absent returns the whole set.
  - `offset`: skip this many entries before the page. Absent is 0.
  - `scope`: `all` (default, every mounted repo's vocabulary, unchanged) or `own` (only the user's OWN repos — an editable authoring surface, the entry or an `edit` member, role-derived — hiding every dependency and the `au.engine.*` builtin). Orthogonal to `repo`: a named dependency repo under `own` returns an empty set. **`schema_version` 11.**
  - a type owned by no mounted member (only borrowed copies present) does not appear in the workspace read, matching `subtypes`.
- no total rides the array; a consumer pages until a short page (`< limit`), or asks `type_counts` for the total.
- result: `{ types: [...] | null }`, type-defs (full or summary); null for an unknown `repo`.
- a `summary` entry carries only `repo`, `name`, `hash`, `parents`, `sealed`, optional `doc`, optional `location`, the same values and semantics those fields have on a full entry. The heavy detail (`fields`, `meta_blocks`, `body`, `effective_body`, `source`, and the `abstract` / `required_meta` / `unmet_required_meta` batch) is omitted; the full read or the single `type` read serves it. In summary mode the heavy payload never crosses the wire and the engine never materializes it.
- a full entry: `repo`, `name`, `hash`, `parents`, `abstract`, `required_meta`, `unmet_required_meta`, `fields` (each `name`, `shape`, `shape_ast`, `required`, optional `key_span`, optional `doc`), `sealed`, `meta_blocks`, `body`, `effective_body`, `source { file, span }`, optional `doc`, optional `brand`, optional `location`.
- `abstract` is a boolean, `true` when the type-def declares `abstract: true`, a non-claimable base (**`schema_version` 18**). `sealed` is separate; a consumer's non-claimable check is `abstract || sealed`. See [[spec - abstract type-defs - a non-claimable open type-def, sealed is abstract plus closed]].
- `required_meta` is a `String[]`, the type-def's own `required:` meta obligations in authored form (with any `::repo`), empty when none (**`schema_version` 18**). Every non-abstract type whose closure includes this def must carry each named meta. See [[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]].
- `unmet_required_meta` is a `String[]`, the required-meta names a NON-abstract type does not satisfy, sorted, empty when satisfied or exempt (**`schema_version` 18**). The read half of the `subtype-missing-required-meta` computation, so a consumer gates without parsing diagnostics.
- `hash` is the type's identity, the closure-hash half of its `TypeId`, hex, content-complete over the referenced closure. Two same-named results across repos are the SAME type iff `(name, hash)` match, so a consumer distinguishes them inline. Same hash as `instances_of`.
- `repo` is the repo the entry is reported from: the owner for the workspace read, the scoped repo for a per-repo read.
- `doc` is the `#:` docstring, advisory and never validated; on a field it is the field's own, on the type-def it is a leading `#:` block before the first key. Absent when none. See [[type docstring::au-type-system]].
- `brand` is present when the type-def declares a `shape:` instead of `fields:`, naming a scalar, enum, union, or tuple as a reusable type (additive, no `schema_version` bump). Absent for a record; a brand's `fields` is empty.
  - `brand.shape` is the underlying shape as a `WireShape` (below), the same AST a field's `shape_ast` uses. A named enum renders as `WireShape::Enum { members }`.
  - `brand.member_docs` is a `{ member: doc }` map of per-enum-member `#:` docstrings, omitted when empty. Advisory. See [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
- `location` is present when the type-def declares a `location:` block, where its instances live (additive, no `schema_version` bump). Absent when none. Advisory, out of the identity `hash`. On both the full and summary entry.
  - `location.name` is the name-template raw source (`"${.type} - ${.slug}"`), omitted when no `name`. `location.path` is the path-glob raw source (`"**/plan/"`), omitted when no `path`. `location.file_type` is `"md"` or `"yaml"`, omitted when no `fileType`. `location.strict` is a boolean, `true` for a mandatory placement (a mismatch is `location-strict-violation` not `location-mismatch`). See [[spec - location constraints - a name template and path predicate as an advisory placement meet]].
- `shape` is the source-form slot expression, a string (`String`, `[low, moderate]`, `decision*[]`).
- `shape_ast` is the same shape parsed, a `WireShape` (below), or null when the shape does not parse.
  - additive beside `shape`; a string-only consumer ignores it.
  - null leans on the `shape-syntax-error` diagnostic, which carries the failure span.
- `key_span` is the field-name byte span in the owning type-def's source, a `SpanRange` (`{ start, end, line_col? }`, file-relative), for go-to-def onto the field's declaration line.
  - the field's FILE is the type-def's `source.file`, so the span alone suffices.
  - absent on the `type_closure` fields, whose entries are gathered across origins without a per-origin source site.
  - additive; a consumer that does not navigate ignores it. NO `schema_version` bump (a new field is additive).

**type_counts.**
- args: optional `repo`, the same repo filter as `types`, and optional `scope` (`all` default / `own`), the same own-vs-all filter as `types` (**`schema_version` 11**). No `summary` / `limit` / `offset`, counts are over the full set, so those are rejected as unknown args.
- result: `{ type_counts: { total, by_repo } | null }`, the shape of the vocabulary without materializing every def, the type dual of `diagnostic_counts`; null for an unknown `repo`.
  - `total`: the count over the same entry set the `types` read spans for the same `repo` scope. Absent `repo` is the owner-deduped workspace set; a present `repo` is that member's whole graph, borrowed copies included.
  - `by_repo`: a map of repo to count, name-sorted. Absent `repo` histograms the owner repos; a present `repo` has the single scoped-repo entry.
- lets a consumer show "42 types across 3 repos" and page `types` until a short page, instead of pulling the whole set to count it.

**type.**
- args: `name`, bare (`foo`) or `::repo`-qualified (`foo::repo`), the authored form, no separate `repo` key. An unknown arg is an `error` frame, not a silent result. Mirrors the `instances_of` arg convention; the structured `repo` filter is the enumeration `types`'.
  - bare `foo`: resolve to the owner copy across all members, the single-type dual of the workspace `types` read, so a hover resolves a member-defined type without knowing its repo.
  - `foo::repo`: scope resolution to that member's graph, the identity that repo holds.
- result: `{ type: <def> | null }` — one type-def in the `types` shape, carrying `repo` and `hash`; null when no member owns the name (bare), the type is absent from the scoped repo, or the repo is unknown.
- **`schema_version` 17**: the batch selector moved to its own verb, `type_batch`. One verb answering two cardinalities under one payload key was the envelope rule's only ambiguous case, so it is two verbs and each is mono-shaped.

**type_batch.**
- args: `names`, an array; each bare or `::repo`-qualified exactly as `type`'s. An unknown arg is an `error` frame.
- result: `{ type_batch: [ <def> | null ] }` — ONE entry per requested name, in request order, each the `type` result's payload or null for an unresolved name.
- the detail-on-demand dual of the `types` summary, so a consumer drilling into several summary entries pays one round-trip, not N. **`schema_version` 17**, split out of `type`.

**type_tree.**
- args: optional `scope` (`all` default / `own`), the same own-vs-all filter as `types` — `own` builds the tree over only the user's own repos' types (**`schema_version` 11**).
- result: `{ type_tree: { roots, nodes } }`, the workspace's owner-deduped type-defs as a cross-repo parent/child adjacency forest, owner-annotated per node. Composable from the workspace `types` read plus `parents`, served first-class as the agent-friendly tree form.
- `roots`, the node names with no parent present in the node set (typically no parents at all), name-sorted. Descending `children` from the roots reaches every node.
- `nodes`, one per owner-deduped type-def, name-sorted. Each: `repo` (owner), `name`, `hash` (the identity, same as the `types` view), `parents` (the declared direct parents, verbatim, including any whose owner is unmounted), `children` (direct children present in the node set, name-sorted).
- type-defs form a DAG, a type may extend several parents, so this is adjacency, not a nested tree: a multi-parent node appears once and is referenced by each parent's `children`. Edges are by type name, which is global, so a subtype in one member links to a base owned in another.

**WireShape.**
The `shape_ast` value, a tagged union on `kind` mirroring the engine's parsed slot shape.
- references carry bare names, not nested shapes; only `list` wraps an inner shape.
- the built-in any-repo-file is `reference` with `name: "file"`; there is no `file` kind.
- the built-in no-type slot `any` is `{ "kind": "any" }`; its reference forms `any*` / `any&` are `reference` / `inline-or-reference` with `name: "any"`, not the `any` kind.
- the built-in uninterpreted slot `opaque` is `{ "kind": "opaque" }`; it is inline-only, there is no `opaque*` / `opaque&` (additive, no `schema_version` bump; a consumer that does not know it treats it as an unconstrained slot like `any`).
- the kinds:
  - `{ "kind": "primitive", "name": "String" }` — `String` | `Number` | `Boolean` | `Date` | `DateTime` | `Url`.
  - `{ "kind": "any" }` — the no-type inline slot, bare `any`; no payload. Interpreted (a value of any shape, still read).
  - `{ "kind": "opaque" }` — the uninterpreted inline slot, bare `opaque`; no payload. Inline-only, stored but never read.
  - `{ "kind": "enum", "members": ["low", "moderate"] }` — member order significant.
  - `{ "kind": "reference", "name": "decision" }` — `decision*`, or `file*` / `any*` as `name: "file"` / `name: "any"`.
  - `{ "kind": "record", "name": "decision" }` — a bare-name inline record.
  - `{ "kind": "inline-or-reference", "name": "decision" }` — `decision&`.
  - `{ "kind": "list", "min": 0, "max": 5, "inner": <WireShape> }` — `inner[min..max]` range cardinality; `min` is the inclusive lower bound on the element count, `max` the inclusive upper (omitted = unbounded above). `[]`=`{min:0}`, `[+]`=`{min:1}`, `[n]`=`{min:n, max:n}`, `[..m]`=`{min:0, max:m}` (**`schema_version` 27**).
  - `{ "kind": "union", "branches": [<WireShape>] }` — `<A | B>`, branch order significant.
  - `{ "kind": "intersection", "branches": [<WireShape>] }` — `<A & B>`, branch order significant.
  - `{ "kind": "compound-reference", "mode": "ref", "op": "union", "branches": ["a", "b"] }` — `<a | b>*`; `mode` is `ref` | `inline-or-ref`, `op` is `union` | `intersection`.
  - `{ "kind": "def-reference" }` — `type*`, the unconstrained typed reference to a type-def; no `bound`.
  - `{ "kind": "def-reference", "bound": { "kind": "single", "name": "mcp.tool" } }` — `type<mcp.tool>*`.
  - `{ "kind": "def-reference", "bound": { "kind": "compound", "op": "union", "branches": ["a", "b"] } }` — `type<a | b>*`; `op` is `union` | `intersection`.
  - `{ "kind": "pinned", "inner": <WireShape> }` — the `*@` enforced-pinned postfix; `inner` is the wrapped `*` reference shape (`file*@`, `T*@`, `<a | b>*@`, `type<T>*@`). `@` attaches only to `*`, so `inner` is never a `&` inline-or-reference (`T&@` is a shape-syntax error). The second wrapper kind beside `list`. Every value must carry a `@commit` pin.
  - `{ "kind": "refined", "base": "Number", "refinement": { "lower": {"value": "0", "inclusive": true}, "upper": {"value": "10", "inclusive": false}, "integer": true, "pattern": "^[a-z]+$" } }` — a value refinement `Base{predicate}`; `base` is the refinable primitive name, `refinement` carries at most one `lower` / `upper` comparison bound (`inclusive` is `>=`/`<=` vs strict `>`/`<`), an `integer` flag (`Number`), and a regex `pattern` (`String`). Absent members are omitted (**`schema_version` 27**).
    - PRODUCER INVARIANT: the `base` determines which `refinement` members can appear. `String` carries only `pattern`; `Number` only `lower` / `upper` / `integer`; `Date` / `DateTime` only `lower` / `upper`. The engine never emits any other combination, so a consumer may narrow the refinement type by `base`. The wire keeps the flat bag (faithful to the engine's internal refinement meet, additive for future predicates) rather than a base-tagged union.
  - `{ "kind": "tuple", "elements": [<WireShape>] }` — a tuple `(A, B, ...)`, a fixed-arity positional product; `elements` are the per-position shapes, order significant. Additive, no `schema_version` bump (a new `kind` value on an existing field).

**instances_of.**
- args:
  - `type`, a type name, bare or `::repo`-qualified.
  - `origins`, optional, an include-set of site kinds to return, any of `file` / `nested` / `meta`. Absent means ALL (**`schema_version` 13**).
  - `instance`, optional boolean, default false. Splice each match's resolved view (the `instance` read's payload) onto its record (**`schema_version` 17**).
  - `body`, optional boolean, default false. Splice each match's markdown BODY (the prose after frontmatter) onto its record (**`schema_version` 17**).
  - an unknown arg is an `error` frame, never silently ignored: a mistyped `instance` / `body` must not quietly answer with the fact absent.
- result: `{ instances_of: [...] }`, one match record per (instance, matched identity), across all requested origins. A bare `type` matches every distinct identity of that name across the workspace; a `type::repo` matches the one identity that repo owns.
- an instance is any typed value conforming to the type, wherever it lives:
  - `file`, a file whose top-level `type:` claims the type.
  - `nested`, a nested inline record inside another instance's field value, at any depth.
  - `meta`, a `meta:` block on a type-def.
- each match record:
  - `path`, the FILE that contains the instance (the instance file, the host instance file, or the host type-def file).
  - `claim`, the instance's effective `type:` claim in authored form; a `::repo` claim stays qualified.
  - `fields`, the instance's own field values as JSON (the record's / meta block's body, the file's frontmatter); the `type:` claim rides `claim`, not here.
  - `name`, the matched type's name.
  - `hash`, the matched type's closure-hash identity, hex. Equal hashes are the same type, so a bare query can return several records for one instance under distinct hashes.
  - `type_owners`, the repos defining this TYPE identity; several when repos share a byte-identical definition (the dedup). Renamed from `owners` (**`schema_version` 17**): the bare name read as "who owns this instance" and misled a consumer, but it answers who owns the TYPE. The instance file's owner is `member`.
  - `member`, always present, the declared name of the workspace member owning the instance FILE. The natural grouping and attribution key, and self-describing beside `type_owners`. A STRING, not the full `resolve_member` record: a consumer needing `root` / `editable` / `role` calls `members` ONCE and joins by name (**`schema_version` 17**).
  - `claimed`, true when the instance directly claims this identity in its `type:`.
  - `inherited`, true when this identity is a transitive ancestor of a type the instance claims.
  - `origin`, the site kind, `file` / `nested` / `meta`.
  - `span`, a `{ start, end, line_col? }` byte range into `path`, always present, for tooling resolution.
  - `locator`, the origin-specific identity within `path`, `null` for a `file` match.
    - `nested`, `{ kind: "nested", field_path, block_id }`. `field_path` is a structured array of field names and list indices, e.g. `["phases", 0, "actions", 1]`. `block_id` is the record's `^` id, or `null`.
    - `meta`, `{ kind: "meta", meta_type, repo }`, the semantic key of the block (`repo` `null` for an own meta type).
  - `doc`, the instance's own `#:` head docstring (a leading `#:` block before its first key), OMITTED when absent. The value-surface twin of the type-def docstrings `types` carries; a plain `#` stays incidental. Additive, a consumer that ignores it is unaffected, NO `schema_version` bump.
  - `field_docs`, an object mapping field name to that field's `#:` docstring, documented fields only, OMITTED when empty. Additive, no `schema_version` bump. Captured on `file` / `nested` / `meta` origins alike; the record-bearing body-fence surface is captured but not yet a read origin (a `body` origin is future).
  - `instance`, present only under `instance: true`, the `instance` read's payload (the resolved view) for `path`.
  - `body`, present only under `body: true`, the file's markdown body (the prose after frontmatter), or `null` when the file is unreadable or opens an unterminated frontmatter. A file with no frontmatter is all body. Named `body`, not `content`: the whole-file text is the standalone `content` read, and nobody pulls it per match, so the enrichment serves only the body a consumer would otherwise strip by hand.
- the `instance` / `body` flags exist so a consumer walking the typed graph fetches adjacent facts in ONE round trip, not `1 + kN`. Opt-in server-side composition of an already-computed (or one-disk-read) fact, the same shape as `ignores`' `resolve?` and `candidates`' `summary?`. The two are independent, either / both / neither.
- each fact keys by the match's FILE. For a `nested` / `meta` match that is the CONTAINING file, not the sub-record, so the facts are file-level. Computed ONCE per unique path and shared across the records that name it. `body` splits the file with the same `split_frontmatter` the parser uses, so it never disagrees with `content` on the boundary.
- BREAKING vs schema 12. The default now returns `nested` + `meta` matches in addition to `file`; pass `origins: ["file"]` for the old file-only stream. Each record gains `origin`, `span`, and `locator` (**`schema_version` 13**).
- BREAKING vs schema 5. The flat `{ path, claim, closure, fields }` per-instance shape was replaced by per-(instance, identity) match records. `closure` was removed; `name` / `hash` / `owners` / `claimed` / `inherited` were added; `claim` is `::repo`-qualified.

**imports.**
- **`schema_version` 17**: renamed from `list_imports`, the only `list_`-prefixed verb in a catalog of nouns.
- args: optional `scope` (`all` default / `own`). `own` keeps only imports MADE BY the user's own repos and drops the automatic `au.engine.*` builtin fold (owned by the builtin, imported by every repo), which is not a user-authored dependency edge (**`schema_version` 11**).
- result: `{ imports: [...] }`, one record per (importing repo, imported peer identity), the fold-axis `::repo` use set discovered across the workspace. A field-shape `foo::repo*` is a reference (the seam), not an import, and is excluded.
- each record:
  - `importer`, the repo whose files authored the `::repo` use.
  - `name`, the imported peer type's name.
  - `owner`, the peer repo it is imported from, the `::repo` as authored.
  - `hash`, the resolved closure-hash identity, hex; equal hashes are the same type.
- an unresolvable `::repo` (undeclared / unmounted / not-found) is excluded here; the gate diagnostics (`type-repo-*` / `peer-type-not-found`) own that feedback.

**Cross-repo `::repo` in served string fields.**
- a `::repo`-qualified type name now appears verbatim in served strings, not stripped to its base.
- carried by the instance `type:` claim (`instances_of`, `instances`, and `instance` `claim`, the `frontmatter` read's `type`, a nested inline-record `type:`, `record_block_ids`'s `claims`, and `resolve_block_id`'s `type_claim`), a type-def parent claim, a type-def sealed claim, a `use:` target, a meta `type:`, and the `semantic_tokens` claim / parent / meta token names.
- carried too by the instance `closure` field (`instances` and `instance` / `resolved`), the type names an instance conforms to, its claim plus every transitive ancestor. Each entry is rendered RELATIVE TO THE INSTANCE'S OWN REPO: an identity the instance's own repo defines is bare, an identity only a peer owns keeps its `::repo` (e.g. `note::base`). The builtin `au.engine.*` ancestors every engine-schema instance carries surface as `au.engine.repo::au-engine` and the like. Conformance to this rule, no `schema_version` bump.
  - owner-relative, NOT verbatim like `claim`. The two usually agree, but for an IN-SYNC same-name identity that the instance's own repo AND a peer both define (one folded identity), a `closure` entry is bare (the own name) even when the `claim` wrote it qualified (`note::base`). Both name the same identity, so a consumer resolving either lands on the same type.
  - a cross-repo DIAMOND, an instance whose closure reaches two DIVERGENT same-named identities (its own `note` and an imported `note::base`, distinct closure-hashes), lists BOTH as distinct entries. So `closure` may carry two strings sharing a base name, one bare, one `::repo`-qualified, and a consumer must not dedupe by base name.
- a folded peer field's shape string reads `foo::repo*`, not `foo*`.
- a name never contains `:`, so the `::` unambiguously separates the type name from the repo scope; consumers parsing these strings must accept the `name::repo` form.

**Cross-repo `::repo` in read arguments.**
- a repo enters an argument three ways, one per question, never interchanged.
- naming ONE type or target uses the authored `name::repo` embedded in the name arg, so a consumer passes the handle it holds without concatenating: `type`, `instances_of`, `subtypes`, and `validate_value`'s `type_name`. A bare name conflates by name; a `name::repo` scopes to the one identity that repo holds.
- scoping an ENUMERATION to a repo uses a structured `repo` filter, "list what this repo holds": `types` (absent = the workspace owner-deduped set, present = that member's graph).
- supplying a VIEWPOINT for a transient input uses a structured `repo`: `validate_value` validates its value against `type_name` resolved in that repo's graph, the explicit form of the per-source-file context a file-based read gets from its path. Absent, it validates against every mounted identity of the name (multi-fit), not a root-repo default.

**type_closure.**
- args: `name`, bare (`foo`) or `::repo`-qualified (`foo::repo`); optional `repo` (the same scoping, as an arg — a `::repo` in `name` says the same thing and wins).
- result: `{ type_closure: [...] }` — the resolved ancestor closure and effective field set of the named type identity.
- MULTI-FIT: a bare `name` conflates across mounted repos, so it answers one closure PER matching identity, each owner-and-hash qualified; it does not guess a winner. A qualifier scopes to the 0-or-1 identity that repo owns. An unknown name is an EMPTY array — the zero case reached the same way as the one and N cases, never a null or an error.
- each entry:
  - `identity`: `{ name, repo, hash }`, the type this closure is for.
  - `ancestors`: `[{ name, repo, hash }]`, SELF FIRST then name-sorted. Each is OWNER-RESOLVED to the repo that actually owns it, so a `parent::repo` edge reports the peer rather than the importing member.
  - `fields`: the effective field set, own fields plus every ancestor's, deduped by name and name-sorted. Each carries `name` / `shape` / `shape_ast` / `required` / `doc` exactly as `types` renders them (but NOT `key_span`, gathered across origins without a per-origin source site), plus `origin: { name, repo, hash }`, the type-def that DECLARES it.
- one traversal answers the three queries consumers were each re-deriving client-side: effective fields, field origin (go-to-definition on a field key), and ancestor / kind membership. It is the same walk the validator runs, so the read cannot drift from what actually validates.
- a field auto-unified across several origins reports the lex-min one, the canonical choice the validator makes. A DIVERGENT field ([[type-def fields collision - auto-unify and qualified field::au-type-system]]) is PRESENT, reporting its canonical (lex-min) origin here; its per-origin shapes are on the instance-level `effective_shape`. **`schema_version` 28**: previously such a field was excluded from this read.
- **`schema_version` 17**: NEW. Cross-repo names made the client-side walks fragile — every consumer had to re-key its walk by `(name, owner-repo)` identity to follow a qualified parent.

**subtypes.**
- args: `base`, bare (`foo`) or `::repo`-qualified (`foo::repo`). The two forms scope differently, mirroring `instances_of`. Optional `scope` (`all` default / `own`), the same own-vs-all filter as `types` — `own` keeps only subtypes owned by the user's own repos (**`schema_version` 11**).
- result: every type-def across the workspace whose parent closure includes `base`, the type-level dual of `instances_of`.
- `base`, the base echoed verbatim (including any `::repo`).
- `subtypes`, name-sorted, one per matching type-def. The base itself is excluded.
  - each carries the owner `repo` beside the same fields the `types` read yields: `name`, `hash`, `parents`, `abstract`, `required_meta`, `unmet_required_meta`, `sealed`, `fields`, `meta_blocks`, `body`, `effective_body`, `source`. The `hash` lets a consumer regroup a bare-conflated result set by identity inline.
  - workspace-wide: it walks every member's graph, so a consumer gets "all subtypes of X" in one read instead of a per-repo `types` enumeration.
  - **bare base** matches by NAME, conflating every same-named identity across the workspace. The "you didn't say which" answer. A name that resolves to no real identity (e.g. only a dangling `extends:` reference, itself diagnosed) has no subtypes and returns empty.
  - **qualified base** scopes to the one IDENTITY that repo owns (`(name, closure-hash)`). Included: the base's OWN repo subtypes, subtypes in importing members (the fold resolves their `parent::repo` edge to that identity), and in-sync vendored holders. Excluded: a different repo's same-named-but-divergent type. An unresolvable `foo::repo` (repo or name absent) returns empty.
  - cross-repo-fold-aware either way: a def that extends `base` only through a `parent::repo` edge (the local copy dropped, a pure import) surfaces here. See `CLAUDE.md`, "ARCHITECTURE", for the per-repo-names / cross-repo-coexistence model this rests on.

**instances.**
- args: none.
- result: `{ count, aborted_at_load, instances: [...] }`. The payload is `instances`; `count` and `aborted_at_load` beside it are envelope metadata. **`schema_version` 17**: the payload key was `entries`.
- `count`, the number of parsed instances.
- `aborted_at_load`, true when a broken vocabulary skipped instance validation, so `entries` is empty for that reason rather than an empty knowledge base.
- `instances`, one per resolved instance: `file`, `claim`, `closure`, `effective_shape`, `effective_values`, `section_presence`, `body_events`, `record_block_ids`.
- `effective_shape`, one entry per field: `field`, `shape`, `required`, `divergent`, `origins`. Each `origins` entry carries `name` (bare), optional `repo` (the owner for a folded peer, so two divergent same-named origins `note` vs `note::base` stay distinct), `origin_path`, and the origin's OWN `shape` / `required`.
  - `divergent` ([[type-def fields collision - auto-unify and qualified field::au-type-system]]) is `true` when the origins disagree on shape: the field is kept and resolved per-origin by a `field{type}` qualifier. The top-level `shape` is then one origin's (no single shape) — read the per-origin `origins[].shape`. **`schema_version` 28**: divergent fields are now PRESENT here (previously excluded and surfaced on a separate `collisions` field, which is REMOVED); each `origins` entry gained `repo` / `shape` / `required`.
- `closure`, the type names the instance conforms to (its claim plus transitive ancestors), rendered OWNER-RELATIVE: an ancestor the instance's own repo defines is bare, one only a peer owns keeps its `::repo` (e.g. `note::base`). Owner-relative, NOT verbatim like `claim`, and a cross-repo diamond lists both divergent same-named identities as distinct entries, so a consumer must not dedupe by base name. See the `closure` rule under the cross-repo-qualifier section above. No `schema_version` bump.
- `record_block_ids`, the addressable inline records: `id`, `claims` (effective, explicit or slot-pinned), `span`. Omitted when none. Body-side markers and fence ids ride `body_events`; together the two lists are the file's addressable-id surface.
- inline records inside JSON-rendered values carry their `^:` id under the literal `"^"` key, beside the `"type"` discriminator — a consumer maps `[[^id]]` references onto received records without re-parsing the file.
- per-file diagnostics ride the `diagnostics` read, not duplicated here.

**candidates.**
- args, all optional, an unknown arg is an `error` frame. The scan is knowledge-base-wide, so there is no `repo` scope.
  - `summary`: `true` projects each file to a lightweight form (`file` + the candidate type names) instead of full per-candidate detail. Absent / `false` is full.
  - `limit`: at most this many files, after `offset`, in the catalog's sorted order. Absent returns every scanned file.
  - `offset`: skip this many files before the page. Absent is 0.
- result: `{ aborted_at_load, candidates: [...] }`, the implicit-identity candidate scan grouped by file. **`schema_version` 17**: the payload key was `files`.
- `aborted_at_load`, true when a broken vocabulary skipped the scan.
- `candidates`, one per scanned instance (present even with no candidates, so scanned-and-empty is distinct from not-scanned, and pages like any other file): `file`, `candidates`.
- a full candidate: `type_name`, `scope { file_path, inline_path }`, `satisfied_required` (field names), `also_satisfied_optional`, `supersedes`.
- a `summary` file's `candidates` is a flat array of the candidate `type_name`s in ranked order; the per-candidate detail (`scope`, `satisfied_required`, `also_satisfied_optional`, `supersedes`) is dropped. Pair with `candidate_counts` for the totals.

**candidate_counts.**
- args: none, the scan is knowledge-base-wide and counts are over the full set, so any arg is an `error` frame.
- result: `{ candidate_counts: { aborted_at_load, total_files, files_with_candidates, by_type } }`, the shape of the scan without materializing every file's candidates, the candidates dual of `diagnostic_counts`.
  - `aborted_at_load`: true when a broken vocabulary skipped the scan, matching `candidates`, so a zero count from a skipped scan stays distinct from an empty one.
  - `total_files`: every scanned file (candidate-bearing or not), reconciling with the `candidates` read's file count.
  - `files_with_candidates`: files with at least one candidate.
  - `by_type`: a map of candidate type name to the number of FILES it is a candidate for ("N untyped files could claim type X"), name-sorted. A type is counted once per file even if it is a candidate at several scopes there.

**instance_counts.**
- args: optional `repo` and optional `scope` (`all` default / `own`), the SAME shape and defaults as `type_counts`. Scoping filters the instance SITE by the repo it lives in (where the instance is authored), not the type identity's owner. No `summary` / `limit` / `offset`, counts are over the full set, so those are rejected as unknown args.
- result: `{ instance_counts: { aborted_at_load, total, by_type } | null }`, per-type instance counts, the instances dual of `type_counts`; null for an unknown `repo`.
  - `aborted_at_load`: true when a broken vocabulary aborted a repo's load, matching `candidate_counts` — closures can be incomplete then, so a count from an aborted load stays distinct from a settled one.
  - `total`: total AUTHORED instance SITES in scope — file-level instances and nested inline records — each counted once regardless of how broad its closure is. Type-def `meta` blocks (the `Meta` origin `instances_of` also serves) are EXCLUDED: they are type-def annotations, not browsable documents, and the always-present `au.engine.*` builtins' meta would otherwise inflate every overview. Engine-schema config-file instances (a repo's `.arsumbris/repo.yaml` is an `au.engine.repo`, a `workspace.yaml` an `au.engine.workspace`) ARE authored instances and DO count, consistent with `instances` / `instances_of`.
  - `by_type`: an ARRAY of `{ name, hash, type_owners, count }`, sorted by (name, hash). Identity-keyed (name + closure-hash), NOT bare name, so same-named cross-repo identities stay distinct and a consumer joins counts onto the identity-keyed `types` rows; `type_owners` is the repos owning a def with that identity, like `instances_of`. See [[spec - cross-repo identity on the wire - a name conflates, a qualifier scopes to identity, every result carries owner and hash]].
  - CLOSURE-INCLUSIVE: a site counts toward EVERY type in its closure (claim + ancestors), so an `article` (which `extends note`) counts toward both. A `by_type` count therefore equals the length of the matching `instances_of` drill-in over the file/nested origins, and the counts do NOT sum to `total`. This is what keeps a "N instances" row label honest against the drill-in.
- lets a consumer render a browsable vocabulary-with-counts overview in ONE round-trip, instead of N `instances_of` reads for N types.
- **Additive, no `schema_version` bump** (a new read, like `hubs`; the precedent is the `doc` / `field_docs` fields).

**instance.**
- args: `path`.
- result: `{ instance: {...} | null }` — one instance's resolved view, null when the path is not a parsed instance.
- **`schema_version` 17**: renamed from `resolved`. A past participle in a catalog of nouns, and it collided with this view's own `resolved` boolean field. `instance` / `instances` is now the same singular/plural pair as `type` / `types`.
- `resolved` (false for an unresolved claim), `claim`, `closure`, `candidates`.
- `effective_values`, the per-field value containers with full contribution provenance.
  - a contribution's `surface` is one of `frontmatter` | `body_wikilink` | `body_fence` | `body_inline_code`. **`schema_version` 19**: `body_yaml_block` was RENAMED to `body_fence`. A marked fence is the multi-line CARRIER and its content-form comes from the slot, so the surface names the carrier, never a content type. See [[type-instance body contribution::au-type-system]].
  - **`schema_version` 19**: a `body_fence` contribution's `kind` now follows the SLOT. A `String` or `any` slot yields `{ kind: "scalar" }` holding the verbatim content where it previously yielded `{ kind: "inline_record" }`. A record-bearing slot is unchanged.
  - a contribution value is one of `{ kind: "scalar", value }`, `{ kind: "reference", target, anchor, block_id, repo, commit }`, `{ kind: "inline_record", fields }`, `{ kind: "tuple", elements, brand }`, `{ kind: "malformed_reference", raw }`, `{ kind: "malformed_constructor", raw }`.
  - a `tuple` carries `elements`, an ordered list of `{ value, brand? }` (each `value` a nested contribution value, recursive), and an optional outer `brand` (the tuple brand's written name, e.g. `"point"` for `point(20, 30)`; absent for the nameless inline form `(20, 30)`). An element's own written brand (e.g. `"color"` for a `color(1)` element of `rgb(color(1), color(2), color(3))`) sits on the element's `brand`, NOT duplicated onto its inner `value` — read a tuple element's brand from the element, and the inner `value` of a tuple element carries no `brand` of its own. The inline value form is the PAREN form `(a, b)` or a `Name(a, b)` constructor — a `[...]` bracket is always a LIST, never a tuple (decision A, see [[type-def shape tuple::au-type-system]]). Where a tuple value previously read as a `scalar` (a raw constructor string, or a bracket array), it now reads structured. Additive value kind, **no `schema_version` bump**. On a `reference`, `block_id` is the `{ id, referent }` object (null when the link carries no `^`), `repo` is the `::repo` qualifier for a cross-repo target, omitted for an own-repo link — mirroring `references_out` — and `commit` is the `@commit` pin for a commit-pinned reference (`[[bar::@a1b2c3d]]`), omitted for an unpinned link.
  - **`commit` on a `reference` is additive, no `schema_version` bump.** A pinned reference previously dropped its `@commit` on this surface; it now round-trips it, mirroring the `references_out` `commit`. See [[type reference::au-type-system]].
  - an `inline_record` carries `fields`, an ordered list of `{ field, values }` where `values` is that nested field's list of resolved contribution values (one for a scalar slot, N for a list slot), each a recursively-resolved contribution value. So a tuple / brand / reference INSIDE a nested record reads resolved exactly like a top-level field, to any depth. **`schema_version` 29, BREAKING**: this REPLACES the former `{ kind: "inline_record", value }` that carried the nested mapping as a raw JSON object (whose inner tuple / brand / reference values were unresolved strings). A nested field whose record type does not resolve degrades to faithful untyped `scalar` values, never absent. See [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
  - a `malformed_reference` carries the `raw` surface of a wikilink-shaped value at a reference slot that did not parse (an out-of-order fragment, a non-oid pin, an invalid field name). Where such a value previously read as `{ kind: "scalar" }` holding the raw string, the total value model now names it, so a read never surfaces a lossy reference. The malformed-wikilink diagnostic is unchanged, it fires from the validator as before. Additive value kind, **no `schema_version` bump**. See [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
  - a `malformed_constructor` carries the `raw` surface of a value that starts a `Name(...)` constructor at a brand slot but does not close well-formed (`meter(42`). Where it previously read as `{ kind: "scalar" }` holding the raw string, the total value model now names it. The `malformed-constructor` diagnostic (a warning) is unchanged. Additive value kind, **no `schema_version` bump**.
  - a `scalar` value optionally carries `brand`, the brand constructor NAME the author wrote (`"meter"` for `meter(5)`, a peer brand keeps its qualifier `"meter::units"`), the discriminator at a union brand and a round-trip signal elsewhere. The `value` is ALWAYS the resolved underlying form (`meter(5)` and a bare `5` both yield `value: 5`), so `brand` is absent for a bare value and for a reserved-primitive escape (`String("x")`, not a brand). Present on both the container's collapsed value and the contributions. Additive, **no `schema_version` bump**. See [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
  - a contribution optionally carries `qualifier`, `{ type_name, repo? }`, the collision qualifier a body attribution wrote (`` `[:field{type}]` ``, `[[x:field{type}]]`, or a ```[:field{type}] fence`) naming which divergent origin it fills ([[type-def fields collision - auto-unify and qualified field::au-type-system]]). Omitted for a bare attribution or a frontmatter contribution. **`schema_version` 28**.
- `section_presence`, top-level declared sections and their presence, null when the type carries no body.
- `body_events`, the raw markdown event stream, null for pure-YAML instances.
- `record_block_ids`, the addressable inline records, same shape as on `instances`. Omitted when none.
- `diagnostics`, this file's diagnostics.

**references_out.**
- args: `path`.
- result: `{ references_out: [...] }` — EVERY outgoing wikilink edge of the file, docstring, frontmatter, and body, each classified. Docstring edges first (a type-def has only these), then frontmatter edges (declaration order), then body edges in source order.
- each: `target`, `resolved` (the file path or null), `span { start, end, line_col }`, `surface` (`frontmatter` | `body` | `docstring`), `kind` (below), optional `repo`, `commit`, `anchor`, `block_id`, `field`, `source_block_id`.
- a new `surface`, `docstring`, for a `[[...]]` in a `#:` docstring on a type-def or instance declaration. Navigational only, so its `kind` is `navigational`; the `surface` is what tells a documentation reference from a prose one. `field` names the documented declaration's field (absent for a head docstring). A type-def now has outgoing edges for the first time, its docstring links. Additive, a NEW edge class carrying a new surface value, no existing edge changes, so NO `schema_version` bump. See [[type docstring::au-type-system]].
- **`schema_version` 17**: this read was BODY-ONLY. Typed frontmatter references surfaced only via the `instance` read, and a `[[...]]` inside a frontmatter string was absent from the forward direction entirely, so no single read answered "what does this connect to". It is now complete and enveloped, and every edge carries `surface` + `kind`.
- **`schema_version` 22**: a new `kind`, `commit-referent`, for a commit-only reference (`[[::@sha]]` / `[[::repo@sha]]`) that names a commit rather than a file. Its `resolved` is null and `commit` is set, but it is NEVER dangling — the commit stays readable because git history is append-only.
- **the kinds.** Load-bearing, not cosmetic: the kind decides how a consumer acts on the edge and how severe a break is.
  - `field-reference` — a frontmatter value in a slot that ADMITS a reference (`myType*` / `file*` / `any*`, or a reference branch of a compound), where the value is exactly one `[[...]]`. The structural edge: validated and closure-checked, and a dangling one is an error. A consumer traverses this as a real dependency.
  - `field-string-wikilink` — a `[[...]]` in a frontmatter value whose slot does NOT admit a reference, or one embedded in a longer string. An intended-but-untyped pointer, navigational only. Includes links in EXTRA fields (outside the effective shape), which have no slot at all.
  - `contributing` — a body `[[target:field]]`, both a link and a data contribution. Following it says where a field value came from; `field` names the field it supplies.
  - `navigational` — a body prose `[[...]]` with no attribution. A hint; a dangling one is a warning.
  - `commit-referent` — a commit-only reference (`[[::@sha]]` / `[[::repo@sha]]`) that names a COMMIT, not a file. `resolved` is null and `commit` is set; the commit stays readable because git history is append-only, and the edge is NEVER dangling, so a consumer must not treat its null `resolved` as a broken link. Surface-independent, settled by the link's own syntax.
  - `unknown` — a WHOLE-VALUE frontmatter wikilink on a TYPED instance whose slot could not be resolved, so the engine cannot say which of the first two it is. It reaches you when the instance's `type:` claim does not resolve, or when its repo's vocabulary aborted. `field-string-wikilink` is a POSITIVE claim that the slot rejects references, so reporting it there would be a lie rather than a shrug. NARROW by construction: it needs a `type:` intent the engine could not honor. A body edge is settled by the link's own syntax, an embedded frontmatter link at the value level, and a plain note is DEFINITIVELY untyped — none of those can be `unknown`.
- **the frontmatter split is decided by the SLOT, never by the value's syntax.** `rel: "[[a]]"` and `see-also: "[[a]]"` are syntactically identical and classify differently, because `rel` is `note*` and `see-also` is `String`. The same rule the value layer applies.
- a list slot yields one edge PER ELEMENT, matching how the value layer splits a list.
- an untyped NOTE (no `type:` claim) reports its FRONTMATTER edges too (notes carry frontmatter), each a whole-value link `field-string-wikilink`, never `unknown`: a note is definitively untyped, so no slot can admit a reference — knowledge, not uncertainty. `unknown` is only for a TYPED instance whose slot the engine could not decide.
- `source_block_id` names the enclosing inline record an edge originates from, mirroring the field of the same name on `references_in`.
- the LOCAL form (`[[^id]]`, `[[#head]]`, an empty target with a locating fragment) resolves to the source file itself, per [[type reference::au-type-system]]'s Local form: resolution skips name lookup, so it has no missing outcome and is never reported dangling.
- the COMMIT-REFERENT form (`[[::@sha]]` / `[[::repo@sha]]`, an empty target with a `commit` and NO locating fragment) is distinct from the local form: it resolves to null (it names a commit, not a file), carries `kind: "commit-referent"`, and is never dangling. See [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
- **this read and `references_in` derive from ONE traversal**, so the two directions cannot disagree about which values are scanned or how a target resolves. They differ in exactly one way, deliberately: the read reports DANGLING edges, the index does not. The index is the write path's reference map — `rename` rewrites referrer bytes off each edge's span — so an unresolved edge must not enter it.
- `block_id` is an object `{ id, referent }`, not a bare string. `referent` is `true` for a `^^id` block-referent (the block's value fills the slot), `false` for a bare `^id` navigational anchor (the file is the referent, `^id` a jump anchor). See [[type block-id::au-type-system]]. Absent when the link carries no `^`.
- `repo` is the `::repo` qualifier when the link crosses a repo boundary; absent for an unqualified link. `resolved` then points into that repo (null when the repo is unavailable or the target is missing there).
- `commit` is the `@commit` pin, the commit-ish a pinned reference resolves against; absent for an unpinned link. It binds to `::repo` (`::repo@commit`, or `::@commit` for this repo). The resolved-edge mirror of `body_events.wikilink.parsed.commit`, so the outgoing-edge set carries which edges are pinned without a per-file body scan. A commit-pinned reference, see [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
  - **A commit-bearing edge is an INERT PIN, so `resolved` is null and it is NEVER dangling, whatever its `kind`.** A pin is a snapshot into an immutable past, it does not attach to a live file. This holds for a NAMED-target pin (`[[file::@sha]]`), which keeps its ordinary `kind` (`field-reference` / `navigational` / …), as well as the empty-target `commit-referent`. A consumer MUST check `commit` before reading a null `resolved` as a broken link; the same `commit` field distinguishes a pin from a genuine dangling edge in both directions and in `neighborhood`.
- the full wikilink fragment grammar is `target[::repo][@commit][#anchor][^block_id][:field]` (`^^` for a block-referent), canonical order, at most one `::repo` / `@commit`. The raw `body_events.wikilink.parsed` breakdown carries the same fragments as the full `WikilinkRef` (`target`, `repo`, `commit`, `anchor`, `block_id`, `field`); each is a string or null EXCEPT `block_id`, which is the `{ id, referent }` object described above.
- pinned references emit the `value-not-pinned` and `wikilink-empty-commit` diagnostics. A pin is inert, so it never drifts and never re-resolves, no divergence diagnostic fires. Codes are open and additive, so they are not cataloged here, see [[spec - diagnostic codes::au-type-system]].

**references_in.**
- **`schema_version` 17**: renamed from `backlinks`. `references_out` / `backlinks` was an asymmetric pair for one axis; the two now read as one pair and sort adjacently.
- args: `path`.
- result: `{ references_in: [...] }`, the inbound reference edges to the file.
- each: `source`, optional `repo`, `slot`, `surface`, `kind`, `span_start`, `span_end`, `line_col`, `block_id`, `source_block_id`.
- `repo` is the source's repo, present only when the inbound edge crosses a repo boundary (the source is in a different repo than the target); omitted for a repo-local edge. Saves a `resolve_member` round-trip on the absolute `source` path.
- source surfaces, exhaustive: every wikilink-valued frontmatter field — including values nested in sequences and inside inline records (a session-log event's `file: "[[notes/foo]]"` indexes) — every body prose wikilink, every wikilink-valued field inside a marked body-fence record (a ` ```yaml [:field] ` block that parses as an inline record), and every `[[...]]` in a `#:` docstring, including a fence record's own docstrings. A fence-record field reference indexes exactly like the same record written in frontmatter, `surface: "frontmatter"`, keyed by its inner field; a docstring link carries `surface: "docstring"`.
- `slot` is the field key (the innermost key for nested values) or a body link's `:field` attribution; `surface` is `frontmatter` | `body` | `docstring`. For a `docstring` edge `slot` names the documented field, absent for a head docstring (additive, no `schema_version` bump).
- **`kind`** classifies each inbound edge, `navigational` | `contributing` | `field` (**`schema_version` 20**). It is DERIVED from `surface` and `slot`, so it never disagrees with them:
  - `navigational` — a body prose link (no `:field` attribution) OR any `docstring`-surface link. A hint; the explosive class a graph walk excludes past one hop. A `docstring` link is told from a prose one by its `surface`, not its `kind`.
  - `contributing` — a body `[[target:field]]`, both a link and a data contribution.
  - `field` — a frontmatter-surface edge.
  - the inbound partner of `references_out`'s kinds, but COARSER on the frontmatter side: `references_out` splits a frontmatter edge into `field-reference` / `field-string-wikilink` / `unknown`, which needs the referrer's resolved shape. Inbound that split is deferred, so a frontmatter edge is `field`, named by its surface alone. A later refinement subdivides `field` without moving the body kinds. See [[spec - neighborhood read - a bounded n-hop reference walk returning a subgraph of files and addressable blocks]] and its Phase-2 decision.
  - inbound-STRUCTURAL is derivable as `slot != null`, the split `hubs` uses; `kind` is the named form.
- `block_id` is the TARGET fragment, the `{ id, referent }` object (not the source's, not a bare string), `referent` distinguishing `^^id` (block-referent) from `^id` (navigational). Omitted when the edge carries no `^`.
- `source_block_id` is the `^:` id of the inline record the reference lives in, when it lives in one — the edge renders as `[[<source>^<source_block_id>]]`. A bare string, it is a record DECLARATION, not a reference, so it carries no mode. Omitted otherwise; nested records report the innermost id.
- only resolving references index; dangling and ambiguous ones ride diagnostics instead.
- a `[[name::repo]]` link forms a cross-repo inbound edge into the named repo when it resolves there; an unqualified link is repo-local.

**pins.**
- a NEW read, additive (a new verb and a new response `type`), so NO `schema_version` bump. The reverse-by-target lookup for COMMIT-PINNED references. An inert pin forms no inbound backlink (see `references_in`), so "which sources pin this name" is not a backlink question; it is a fold over the retained OUTBOUND pins.
- args: `target` (the pinned name to find), `source_type` (a type name scoping the source set). Both REQUIRED.
- result: `{ pins: [...] }`, every commit-pinned reference naming `target` whose source file contains an instance of `source_type`.
- `source_type` scopes the candidate FILE set to `instances_of(source_type)` — the files containing a matching instance, a top-level claim or a NESTED inline record, both keyed to the containing file — so the fold stays off the whole graph. There is no unscoped whole-graph form; `source_type` is required.
- each: `source`, `source_block_id?`, `span` (the shared `SpanRange`, `{ start, end, line_col }`), `slot?`, `surface`, `target`, `repo?`, `commit`, `block_id?`.
  - `source` is the file holding the pin. `source_block_id` is the `^:` id of the enclosing inline record when the pin sits in one (a nested-record site), omitted otherwise.
  - `slot` is the field the pin fills (the innermost key for a nested value) or a body `:field` attribution, omitted for an untyped prose pin. `surface` is `frontmatter` | `body` | `docstring` (a `[[…::@sha]]` in a `#:` docstring; additive, no `schema_version` bump).
  - `target` is the pinned name EXACTLY as recorded, not live-resolved. `repo` is the `::repo` scope when the pin crosses a boundary. `commit` is the pinned sha, always present.
  - `block_id` is the `{ id, referent }` fragment on the pin, omitted when the pin names no interior block.
- NO time awareness and NO live resolution: a since-reused name returns EVERY pin naming it, the old target and the new alike. The consumer windows by rename time to exclude a since-reused name. The engine narrows by name and source type; the windowed fold is the consumer's. See [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
- an empty-target commit-referent (`[[::@sha]]`) names a commit, not a file, so it never matches a named `target` and never appears here.

**commit_meta.**
- a NEW read, additive (a new verb and a new response `type`), so NO `schema_version` bump. Per-commit metadata for a set of commits, member-aware, off the build path. The git-enrichment join partner for `pins`: a consumer collects the distinct `{ commit, repo? }` set and calls this once. General, not pin-specific — reusable for any commit. See [[spec - engine-mediated git reads - member-aware commit metadata and file history over the object store]].
- args: `commits`, a list of `{ commit, repo? }`. `commit` is a commit-ish (a full oid or an abbreviated prefix). `repo` names the member whose store to resolve it in, PER commit (a folded pin set spans members), defaulting to the entry repo when absent.
- result: `{ commit_meta: [...] }`, one record per input commit, POSITIONAL and in input order, so a consumer maps a record back to its query by position (an abbreviated input resolves to the full `commit` oid).
- each: `commit`, `available`, `timestamp?`, `author?`, `message?`, `trailers`.
  - `commit` is the resolved full oid when present, else the requested input verbatim.
  - `available` is whether the member's store holds the commit. `false` (an unknown repo, a `.git`-free member, a missing sha, or a git failure) omits every metadata field, the shape of `pinned-commit-unavailable`.
  - `timestamp` is the committer date, unix seconds, when the commit entered history. `author` is `Name <email>` (an engine-written commit is `au-engine <au-engine@arsumbris.ai>`). `message` is the raw commit message.
  - `trailers` is every trailer line, `[{ key, value }]`, uninterpreted — the engine's own (`Mutation-Id`, `Moved:`) and a caller attribution line alike. Always an array, empty never absent. ADVISORY, trace-tier: a trailer is unsigned free text, forgeable, and its `sha`-mapping rides the append-only invariant. A consumer treats it as a claim, not proof.
- member-aware: each commit routes to its owning member's covering working tree (the git store), the routing a member-blind `git log` shell-out gets wrong. Metadata-only — the commit's BYTES are the pin-resolution read's job, not this one.

**file_history.**
- a NEW read, additive (a new verb and a new response `type`), so NO `schema_version` bump. A file's commit stream, member-aware, off the build path. Serves an audit enumerating a file's commits, and a commit-stream UI. See [[spec - engine-mediated git reads - member-aware commit metadata and file history over the object store]].
- args: `path` (the file, relative to the entry repo, or to `repo`'s member root when given), `repo?` (the member; absent = the entry).
- result: `{ file_history: [...] }`, the commits that touched the path, NEWEST FIRST (git's log order, a partial order over a DAG — a consumer must not read it as a total order across unrelated branches).
- each: `commit`, `timestamp`, `author`, `message`, `status`, `from?`.
  - `commit` is the full oid. `timestamp` is the committer date, unix seconds. `author` is `Name <email>`. `message` is the commit SUBJECT line (the first line); the full body is a `commit_meta` join away.
  - `status` is `added` / `modified` / `deleted` / `renamed`. `renamed` is git's own SIMILARITY HEURISTIC, best-effort, NOT authoritative — adjudicating a rename is a consumer's lineage tool, not this read. `from` is the prior path on a `renamed`, git's heuristic match, omitted otherwise.
- member-aware: git resolves the covering working tree from the file's own directory, so a file in a member mounted outside the entry reads against its own tree. A `.git`-free member (a package snapshot) or a path with no history yields an EMPTY stream, named, not an error.

**recent_commits.**
- a NEW read, additive (a new verb and a new response `type`), so NO `schema_version` bump. A bounded, newest-first commit stream merged across the workspace's working trees, member-aware, off the build path. `file_history` generalized to no-path / all-trees / bounded, with `trailers` on every row and a per-tree tag. Serves a cross-repo git ACTIVITY view. See [[spec - recent-commits activity stream - a member-aware bounded commit stream with a reflog-watched append-only subscription]].
- args: `members?` (a list of member NAMES; each maps to its owning working tree and is deduped — a monorepo's members share one tree; absent = all trees), `limit?` (a count ceiling), `since?` (a git time window, e.g. an ISO date or `2.weeks`). With NEITHER `limit` nor `since`, a default cap (100) applies, so the read always bounds. Given both, they intersect: commits since the window, newest first, at most `limit`.
- result: `{ recent_commits: [...] }`, the merged commits NEWEST FIRST by committer timestamp, tie-broken by commit oid for a total, deterministic order, cut to the effective bound.
- each: `commit`, `tree`, `members`, `author { name, email }`, `timestamp`, `subject`, `changed_files`, `trailers`.
  - `commit` is the full oid. `tree` is the owning working-tree ROOT (the lane key, paralleling the `members` read's `git.root`), `members` the au-repo member names living in that tree.
  - `author` is `{ name, email }`, split so a consumer tells a human commit from an engine one (`au-engine <au-engine@arsumbris.ai>`). `timestamp` is the committer date, unix seconds. `subject` is the commit summary line; the full body is a `commit_meta` join away.
  - `changed_files` is `[{ path, status, from? }]`, `status` one of `added` / `modified` / `deleted` / `renamed` (git's `-M` similarity heuristic, best-effort, surfaced raw), `from` the prior path on a `renamed`. Empty for a merge commit, whose diff git omits by default.
  - `trailers` is every trailer line, `[{ key, value }]`, uninterpreted, the same advisory trace-tier data `commit_meta` returns; `Mutation-Id` / `Mutation-Members` let a consumer collapse one mutation's cross-tree commits into one feed entry client-side.
- member-aware: each tree's log runs in its own store (one `git log` per tree, never one per commit), merged. A `.git`-free member (a package snapshot) contributes nothing, named. The live form is the `recent_commits` SUBSCRIPTION (below), an append-only stream over a reflog watcher.

**neighborhood.**
- a bounded N-hop reference-graph walk from one seed, returning the reachable SUBGRAPH (nodes plus the edges traversed), not an edge list. `references_out` / `references_in` are the one-hop primitives; this generalizes them, with direction and depth as arguments so one verb covers the whole axis. See [[spec - neighborhood read - a bounded n-hop reference walk returning a subgraph of files and addressable blocks]].
- args:
  - `path`: the seed file, always a file node. Required.
  - `direction`: `out` (default) / `in` / `both`. `out` follows edges the node authors, `in` edges pointing at it, `both` either per hop (a true undirected walk, so depth 2 reaches a co-cited sibling). An unknown value is a malformed-read error frame.
  - `depth`: the maximum hop count, default 1 (the seed plus its direct targets). A node at `depth` is a leaf, discovered but not expanded.
  - `kinds`: an include-set of edge kinds, any of `navigational` / `contributing` / `field` / `commit-referent` (**`schema_version` 22** adds the outbound-only `commit-referent`; see the `edges.kind` note below). Absent means all kinds at depth 1, but is REQUIRED past depth 1 — an unfiltered deep walk explodes through navigational fan-out. An absent `kinds` at `depth > 1`, or an unknown kind value, is an `error` frame naming the accepted kinds.
  - `scope`: `all` (default; a `path` is a location pin) or `own` (prune at the repo boundary — a crossing edge into a dependency is reported but its target not expanded).
  - `max_nodes`: the node cap; absent is the engine's bound. The seed counts, so `max_nodes` 1 returns the seed alone.
  - `content` / `body` / `instance`: independent enrichment booleans, default false. See below.
  - an unknown arg is an `error` frame (`frontmatter` enrichment and `max_bytes` are not built).
- result: `{ neighborhood: { nodes, edges, truncated, truncated_at_depth?, dropped } }`.
- **nodes**, the reachable set, sorted by `(depth, path, block_id)` so two builds return one answer. Each:
  - `path`: the file, or for a block node the file CONTAINING it.
  - `block_id`: present only on a block node, the `^^`-addressed block. A block node exists ONLY as the resolved target of a `^^` block-referent; a bare `^id` reaches the FILE node with its anchor on the edge. A file node and a block node inside it are DISTINCT nodes.
  - `depth`: the MINIMUM hop count from the seed.
  - `repo`: the owning member; omitted when the path belongs to none.
  - `file_kind`: `instance` / `type-def` / `note` / `asset`, the parse kind, matching `hubs` and `files`.
  - `bytes` / `body_bytes`: the byte lengths a consumer costs a FETCH by before paying for it. For a FILE node the whole-file and prose-body lengths; for a BLOCK node both are the block's SPAN length (a block has no frontmatter to strip). `bytes` equals the `content` read's `text` length, `body_bytes` the `body` length, so an estimate matches the payload. `null` for an unread asset, or a block whose `^^` id did not resolve.
  - `content` / `body` / `instance`: present only when the matching flag is set, the payload spliced onto the node, one round trip instead of `1 + kN`. For a FILE node: `content` is `{ text, hash, commit }` from the working tree, `body` the prose after frontmatter, `instance` the resolved view (`null` for a plain note). For a BLOCK node: `content` / `body` are the block's span SLICE (`content` carries `null` `hash` / `commit`, a block is not a git file), and `instance` is `null` (a block has no standalone resolved view). The key is present-when-requested even when null, absent otherwise.
- **edges**, the traversed edges, each stored NATURAL-direction (`from` the referrer, `to` the target), sorted by `(from, span)`. Under `both` one physical edge is reported once. Each:
  - `from`: `{ path, block_id? }`, the referrer node.
  - `to`: `{ path, block_id? }`, the target node; `null` for a dangling edge (an outbound link resolving to nothing), or a scope-/budget-excluded target absent from `nodes`.
  - `kind`: the COARSE walk vocabulary, `navigational` / `contributing` / `field`, derived from `surface` and the slot, symmetric in both directions. Deliberately coarser than `references_out`'s set: the frontmatter split (`field-reference` / `field-string-wikilink` / `unknown`) needs the referrer's resolved shape, so the walk collapses it to `field` and a consumer drills to `references_out` for the finer forward split. A later refinement subdivides `field` without moving the body kinds. Plus `commit-referent`, an OUTBOUND-only kind for a commit-only reference (`[[::@sha]]` / `[[::repo@sha]]`): it reaches no node (`to` is null) but is NOT dangling. So the `kinds` filter accepts `navigational` / `contributing` / `field` / `commit-referent`.
  - `surface`: `frontmatter` | `body`. `span_start` / `span_end` / `line_col`: the reference's byte range in `from` (the referrer).
  - `field`: the slot / `:field` attribution; `source_block_id`: the enclosing inline record's `^:` id; `block_id`: the link's `{ id, referent }` fragment. Each omitted when absent.
  - `repo` / `commit` / `anchor`: the `::repo` / `@commit` / `#anchor` fragments, present only on an OUTBOUND-discovered edge (an inbound edge's backlink index dropped the link). Omitted otherwise.
- **truncation** is loud. `truncated` is true when `max_nodes` cut an expansion that had more; `truncated_at_depth` names where cutting began (omitted when not truncated); `dropped` NAMES each cut node (`{ path, block_id?, repo?, depth }`), not a count, so a consumer surfaces or re-fetches exactly what was lost. Reaching `depth` is NOT truncation. Under truncation every `to` absent from `nodes` appears in `dropped`; a scope-pruned peer is a POLICY exclusion, not budget, so it is reported by its edge (with `to.repo`) but is NOT in `dropped`.
- a NEW read, additive: the read name adds no `schema_version` beyond the `references_in.kind` bump (schema 20) it rides with.

**resolve_target.**
- args: `target`, optional `origin`.
- result: `{ resolve_target: { path, kind, hash, source } | null }`, null when unresolved or ambiguous.
- `path`: the resolved absolute file path.
- `kind`: the engine's classification, `type-def` | `instance` | `repo-registry` | `workspace` | `repo-lock` | `unclassified`.
- `hash`: the file's content hash (null for unread files) — the mutation channel's `expected_hash` source.
- `source`: `{ file, span }` for a `type-def` target, else null. `file` is the openable file path, `span` is a `{ start, end, line_col }` — the same shape the type reads carry. A non-type-def `path` is itself directly openable and `source` is null.
- `target` carries the wikilink fragment grammar, so an embedded `::repo` qualifier is honored — the same resolution `references_out` reports for a body link.
- `origin` is the file the link appears in. It scopes resolution to that source's repo: a bare target resolves repo-local against the origin's index, a `::repo` target resolves cross-repo. This is the multi-member navigation path (go-to-definition); pass the open file as `origin`.
- without `origin`: a `::repo` target resolves against the named repo's index; a bare target resolves scopelessly across every repo index, the single-repo form (a bare target present in two repos is ambiguous → null).
- LOCAL form: an EMPTY `target` with an `origin` resolves to the origin file itself, per [[type reference::au-type-system]] — a wikilink with no name and a locating fragment (`[[^id]]` / `[[#head]]` / `[[^^id]]`) addresses the file it appears in. So `resolve_block_id` / `resolve_anchor` / `block_ids` / `anchors` resolve the fragment against the origin, matching the validator and the backlink index. An empty target WITHOUT an `origin` resolves nothing (no file to be local to). Distinct from the commit-referent (`[[::@sha]]`, empty target with a `commit`), which names a commit and resolves to null — see `references_out`.
- a COMMIT-PINNED target (`[[file::@sha]]` / `[[::@sha]]`) resolves to null: a pin is an inert tombstone into an immutable past, so navigation never jumps to a live file, matching `references_out` (`resolved: null` for any commit-bearing edge) and the backlink index. Applies to `resolve_block_id` / `resolve_anchor` / `block_ids` / `anchors` too, they share this resolution.
- resolution per [[type reference::au-type-system]]: an extensionless target matches any file's stem (`s-001` reaches `s-001.yaml`, `photo` reaches `photo.png`); an extension is only required when the stem alone is ambiguous.

**resolve_block_id.**
- args: `target`, `block_id`, optional `origin`.
- result: `{ resolve_block_id: {...} | null }` — the addressable entity, the one resolution surface, navigation included.
- an inline record carrying `^: id` (checked first, frontmatter precedes the body), else the first body occurrence: a typed `[:field]` fence, an untyped fence id, or a bare `^id` marker.
- `file_path`, `type_claim` (a record's effective claim, explicit or slot-pinned; empty for navigational markers), `span { start, end }`.
- `kind`: `record` / `typed_block` / `marker` — a `marker` is navigational only, never a typed-reference target.
- `target` and `origin` scope the target file's resolution exactly as in `resolve_target`.

**resolve_anchor.**
- args: `target`, `anchor`, optional `origin`.
- result: `{ resolve_anchor: {...} | null }` — the heading, null when unresolved.
- matching is the engine's contract: case-insensitive exact heading text (trailing `^id` markers excluded), first match in document order.
- `file_path`, `span { start, end }` — the heading line.
- `target` and `origin` scope the target file's resolution exactly as in `resolve_target`.

**anchors.**
- args: `target`, optional `origin`, scoping the target's resolution exactly as in `resolve_target`.
- result: `{ anchors: [...] | null }`, every heading in the target file, in document order.
- each: `text`, `level`, `span { start, end, line_col }`.
- the LISTING dual of `resolve_anchor`: that verb answers whether one `#anchor` resolves, this answers what a `#anchor` can address. A consumer completing `[[file#` enumerates instead of guessing.
- `text` is exactly what a `#anchor` fragment matches — the heading text with its trailing `^id` marker excluded, per [[type block-id::au-type-system]]. Serving the matched form means a consumer inserts it verbatim and the link resolves; matching stays case-insensitive, so the served casing is one valid spelling, not the only one.
- `level` is the heading depth, 1..=6, carried because the body scan already holds it — an outline consumer needs no second read.
- `span` is the heading LINE, the same span `resolve_anchor` returns for that heading.
- an entry carries no `file_path`: it is the argument, so repeating it per entry says nothing. What remains is `resolve_anchor`'s payload plus the key that selects it, so list-then-pick costs no second round trip.
- **null vs empty is a real distinction.** An unresolved `target` answers a null payload, the catalog's unresolved-lookup signal. A target that RESOLVES but carries no markdown body (a pure-YAML instance, an unread asset) answers an EMPTY array — it exists and simply has no headings, and collapsing that into null would report "no such file" for a file that is there.
- a NEW read, additive: new reads are new request values, so no `schema_version` bump. See [[spec - addressable enumeration reads - the plural of each resolve verb, one listing per wikilink fragment position]].

**block_ids.**
- args: `target`, optional `origin`, scoping the target's resolution exactly as in `resolve_target`.
- result: `{ block_ids: [...] | null }`, every addressable id in the target file, in document order.
- each: `id`, `kind`, `type_claim`, `span { start, end, line_col }`.
- the LISTING dual of `resolve_block_id`, the `^`-position sibling of `anchors`. A consumer completing `[[file^` enumerates what is addressable instead of scanning the text for it.
- BOTH surfaces of [[type block-id::au-type-system]] in one stream: frontmatter inline records carrying `^:`, and body occurrences (typed `[:field]` fence ids, untyped fence ids, bare `^id` markers). Sorted by span, so frontmatter precedes the body the way the file does. A regex over the body catches only the last of those, which is the gap this closes.
- `id` is the bare id, without its `^` sigil, as `[[file^id]]` spells it.
- `kind` is `record` / `typed_block` / `marker`, `resolve_block_id`'s vocabulary verbatim.
- **typedness rides `kind`, never a separate flag.** A `^^` block-referent demands a typed value, satisfied by `record` and `typed_block` and not by `marker`, so a consumer completing `^^` filters on `kind` and one completing a bare `^` does not.
- `type_claim` is the effective claim, explicit or slot-pinned, `::repo`-qualified where the claim is. Empty for a `marker` and for a claim-less record.
- **every OCCURRENCE is listed, duplicates included.** `resolve_block_id` returns the first and ignores the rest, which is right for resolution and wrong for a listing: dropping the later ones would hide exactly what `block-id-duplicate` reports. So a file carrying one id twice yields two entries, and the two reads deliberately disagree on cardinality — the listing is the file's surface, the resolver is the verdict.
- an entry carries no `file_path`, and the null-vs-empty split is `anchors`': an unresolved `target` is null, a resolved file carrying no ids is an empty array.
- a NEW read, additive, no `schema_version` bump.

**files.**
- args, all optional, an unknown arg is an `error` frame.
  - `repo`: only this member's files. Absent spans every mounted member.
  - `scope`: `all` (DEFAULT) or `own`. See below.
  - `limit`: at most this many entries, after `offset`, over the path-sorted set. Absent returns the whole catalogue.
  - `offset`: skip this many entries before the page. Absent is 0.
- result: `{ files: [...] }`, every catalogued file, path-sorted.
- **path-sorted is COMPONENT-WISE, not byte-lexical.** The order is the catalogue's `PathBuf` key, which compares path segments one at a time, so a directory precedes a sibling file whose name extends it: `content/broken/x.md` before `content/broken example.md`. A consumer re-sorting or merge-joining with a plain string compare disagrees there (`' '` 0x20 < `'/'` 0x2f). Paging slices this order, after the `repo` / `scope` filters.
- each: `path`, `stem`, `repo`, `kind`.
- the resolvable target set for a `[[` wikilink, and the listing dual of `resolve_target`. **The catalogue IS the resolvable set**: the reference index is built from it, so what is catalogued is what a wikilink can reach.
- **an ASSET appears**, unread and hashless. The walker records it by path and never reads it, precisely so `file*` resolves against it, so a consumer completing `[[` can offer a target the validator accepts. The implicit-identity `candidates` scan reaches only PARSED files, so reading that for the file set silently omits every asset — the gap this read closes.
- `stem` is the basename minus ONE extension, what a bare `[[name]]` resolves by: `x.session.yaml` stems to `x.session`, `paper.pdf` to `paper`. Served so no consumer re-derives the strip-one-extension rule, and the extensionless-match rule with it. A `*.type.yaml` keeps its `.type` tail (`task.type`), which is why a wikilink by TYPE-NAME resolves through its own alias rather than the stem.
- `stem` is NOT unique. Two files sharing one are an ambiguous bare target, which resolution reports as `reference-target-ambiguous`; the listing states what exists and does not adjudicate.
- `kind` is `instance` / `type-def` / `note` / `asset`, the shared vocabulary, one derivation with `hubs` and `neighborhood`.
- `repo` is the owning member, null when the path belongs to none. An engine-schema file (`.arsumbris/repo.yaml`, `workspace.yaml`, the locks) is catalogued and resolvable, so it is listed like any other file.
- **`scope` defaults to `all`, not the `own` an actionability read takes.** A wikilink into a dependency is legal — only a TYPE crossing gates on a declared dep — so the resolvable target set is what exists to link against, the vocabulary side of the scope split. Defaulting to `own` would hide targets the validator accepts. A `repo` filter and `scope` compose (AND), as everywhere.
- the READ dual of the `files` subscription channel, which streams the same path set and its deltas: read once, subscribe for change, rather than choosing between them.
- a projection of the held catalogue, so no walk and no disk read.
- a NEW read, additive, no `schema_version` bump.

**dir_entries.**
- **`schema_version` 17**: renamed from `children`, which was ambiguous — `type_tree.nodes[].children` already means graph children in this same API.
- args: `dir`.
- result: `{ dir_entries: [...] }`, the direct entries of a directory, files and subdirectories, hidden excluded. Catalog-derived, so a directory holding no catalogued file does not appear.
- each: `path`, `name`, `kind` (`file` or `directory`).

**frontmatter.**
- args: `path`.
- result: `{ frontmatter: {...} | null }` — the parsed frontmatter as a JSON map, null when the file is neither a typed instance nor a note.
- a typed instance carries its `type:` claim under `type`, a note carries no `type` key.

**content.**
- args: `path`.
- result: `{ content: { text, hash, commit } | null }`, null when unreadable.
- **`schema_version` 17**: the source string is `text`, not `content`. The payload key is the read's own name, so a `content` field inside it would make `result.content.content` the way to the string — the same silent-`undefined` trap one level down. The envelope rule fixes a collision on the INSIDE.
- `text`: the file's source text. The one read that returns source-form text, read from the working tree.
- `hash`: the content hash of those bytes, the guard-usable `expected_hash` for `write_file` / `delete_file`.
  - content and hash come from one read of the bytes, so they are coherent: the hash is the hash of the content returned.
  - present whenever the file is readable, with no dependence on whether a build has catalogued the file. A read on a still-deriving daemon still arms the save guard.
  - it is the value the mutate guard re-hashes on disk, not the catalog hash; a read never consults or warms the catalog.
- `commit`: HEAD of the repo owning the file, the pin anchor for the returned bytes, symmetric with `mutate`'s `result.commit`.
  - null when the file is not under a git working tree, exactly as the mutation path's commit is null off-git. The field is present-but-nullable, never an error.
  - the returned `content` is the working tree's, possibly dirty against HEAD; `commit` anchors a pin (`[[path::@<commit>]]`) without committing, so the bytes may differ from that commit's tree.

**validate_value.**
- args: `type_name`, `value`, optional `repo`.
  - `type_name`: the type-def to validate against. A bare name conflates across mounted repos; a `::repo` in the name scopes to one identity.
  - `value`: an arbitrary JSON value, the transient instance to check (a tool-input map, a config, ...). It is the instance BODY, without the type claim. A `type` key that MATCHES `type_name` is tolerated and ignored, so pasting the on-disk instance shape (which carries its own `type:`) never manufactures a spurious `duplicate-key 'type'` ahead of the real diagnostic. A MISMATCHING `type` still surfaces as a duplicate key, a real contradiction.
  - `repo`: scope to one repo's identity, by its declared name. Absent = every mounted repo owning `type_name`. A `::repo` in `type_name` WINS over this arg, so the two never silently contradict.
- result: `{ validate_value: [ { identity, diagnostics, undeclared_fields } ] }`, one verdict PER mounted identity the name denotes.
  - `identity`: `{ name, repo, hash }`, the `(name, repo, closure-hash)` triple every cross-repo result carries; or NULL, see below.
  - `diagnostics`: the same shape and codes as the `diagnostics` read, the verdict for that one identity.
  - `undeclared_fields`: the value keys not in this identity's effective shape. ADVISORY, undeclared fields are legal under open-world validation, so this is not a diagnostic — it surfaces them so a caller can catch a typo'd extra that quietly passed. EMPTY for a null identity (no shape to compare). The injected `type` claim and qualified `field{origin}` keys are excluded, they are not plain fields.
- MULTI-FIT: a bare name owned by N mounted repos returns N verdicts, each validated against ITS OWN identity's shape — the engine does not guess a winner. A `repo` arg, or a `::repo`, narrows to the 0-or-1 identity that repo owns.
- the verdict a file would get if `value` were its frontmatter claiming that identity: same open-world stance, same catalog.
- the value never touches disk; this is the "validate" half of validation decoupled from "is a file".
- FAILS CLOSED on an unknown name. A name no mounted repo owns — a typo, an unmounted dep, or an unknown `repo` — returns ONE verdict with `identity: null` carrying the `unknown-type-claim` a file claiming an absent type gets, plus any structural diagnostics the value itself has. NOT an empty array: a consumer folding `diagnostics` across the verdicts would read `[]` as "valid" and wave an unvalidated value through, so the miss is a finding, never an absence.
- a non-object `value` returns `instance-not-a-mapping`; frontmatter is a mapping. It surfaces even under a null identity, so an unknown name never hides a malformed value.
- frontmatter only, a value has no body.
- spans index a SYNTHESIZED document, not the caller's value, so `range` and `line_col` do not map back to the input.
  - the codes and messages are the verdict; the spans are diagnostic-internal.
  - related spans into real workspace files keep their own file's coordinates.
- against a repo whose build aborted (broken vocabulary), only the value's structural diagnostics return, matching that files go unvalidated then.

**preview_mutation.**
- args: `op` plus the op's own arguments. `op` is one of `write_file` / `edit_file` / `delete_file`, the deterministic v1 mutations; its args mirror the same-named `mutate` verb.
  - `write_file`: `path`, `content`, optional `stamps`.
  - `edit_file`: `path`, `old_string`, `new_string`, optional `replace_all`, optional `stamps`.
  - `delete_file`: `path`.
  - `stamps` fold into the would-be content exactly as a real write folds them, so the previewed product matches what would land. `expected_hash` is NOT an arg: a concurrency guard is about the write moment, not the product.
- result: `{ preview_mutation: <product> }`, where `<product>` is either the built product or a structural reject.
  - built: `{ target, blast_radius }`.
    - `target`: `{ path, hash, identities, diagnostics }`.
      - `path`: the resolved target path. `hash`: the would-be content hash (the `expected_hash` a later real write can guard on), null for a delete.
      - `identities`: the `(name, repo, hash)` triples the would-be file CLAIMS, each resolved in the file's own repo (a `::repo` claim scopes itself). EMPTY for a plain note (no `type:`) and for a delete. So the pre-tool gate "is this a valid `X`" checks `identities` contains `X` AND `diagnostics` carry no error-severity finding.
      - `diagnostics`: the same shape and codes as the `diagnostics` read, for the would-be file, WHOLE-file (body typing runs too, unlike `validate_value`'s frontmatter-only verdict).
    - `blast_radius`: `[ { path, diagnostics } ]`, one entry per OTHER file whose diagnostics differ from the current knowledge base — a delete's dangled referrers, a type-def edit's broken dependents, a write's fixed referrers. Per-FILE only; a repo-level diagnostic change (a new `duplicate-type-def`) is not projected here (see the spec's Friction).
  - reject: `{ reject: { message, detail? } }`, when the op refuses before any product exists (an edit or delete of an absent file, an absent or non-unique `old_string`, a stamp on a non-list field, a path that mounts nowhere, a path in a consumed non-editable member). A reject is DATA on a successful read, not an error frame.
- it SIMULATES the mutation over an overlay of the current snapshot and rolls it away: no disk write, no git commit. The type and diagnostics come from the same recompute a real mutation's rebuild runs, so the preview cannot disagree with what the write would land. It determines the ACTUAL type from the would-be content, so a `type:` claim in junk cannot pass ([[spec - mutation preview read - simulate a write over an overlay and report its product without committing]]).
- v1 covers `write_file` / `edit_file` / `delete_file`. The structural refactors (`rename` / `promote` / `inline` / `rename_type`) are not previewable in v1.
- additive, so NO `schema_version` bump.

**semantic_tokens.**
- args: `path`.
- result: `{ semantic_tokens: [...] | null }`, a file's ordered token stream; null when the path is not a held file.
- the type-aware, reference-aware highlight tokens a generic grammar cannot produce ([[spec - semantic tokens]]).
- each token: `range { start, end, line_col }`, `kind`, plus kind-specific fields. Sorted ascending by `range.start`.
- the kinds:
  - `wikilink-resolved` / `wikilink-broken` — a body or frontmatter wikilink. `target`, optional `repo` (the `::repo` qualifier when the link crosses a repo boundary), plus `resolved` (the file path) when it resolves. A `::repo` link resolves into the named repo (`resolved` points there); an unqualified one repo-local. Every wikilink the engine sees, broken ones addressable here directly, not only via diagnostics. The LOCAL form (an empty target with a locating fragment, `[[^id]]` / `[[#head]]` / `[[^^id]]`) resolves to the file itself, so `resolved` is that file and the token is never broken.
  - `wikilink-pinned` — a commit-pinned wikilink: a named pin (`[[file::@sha]]`) or the empty-target commit-referent (`[[::@sha]]` / `[[::repo@sha]]`). `target`, optional `repo`, plus `commit` (the pinned sha). An inert coordinate into an immutable past — never re-resolved live and never dangling — so it is neither `wikilink-resolved` nor `wikilink-broken`, mirroring the `commit-referent` edge kind on `references_out`. Additive, consumers that do not know it ignore the kind (no `schema_version` bump).
  - `field-value` — a typed scalar value. `field`, `value_type` (a `WireShape`, above). The declared shape (top-level field, list element, or slot-pinned nested record), else inferred from the YAML literal as a bare primitive. Always present.
  - `type-claim` — a claim referencing a type-def, one per claimed name. `name`. An instance identity `type:`, a type-def `extends:` parent claim, a `meta:` sub-region's `type:`, inline records, and typed fences.
  - `typed-block` — an inline `[:field]` fence, spanning the whole fence. `field`. A container: its inner scalars ride `field-value`, the one nesting.
  - `block-id` — an addressable id. `id`. A bare `^id` marker, a trailing fence id, or a `^:` inline-record id.
  - `anchor` — a heading line, the target a `#anchor` fragment resolves to. `text`.
- the type-def shape kinds, emitted only for a type-def file (`*.type.yaml`):
  - `field-shape` — a field's declared shape, a container spanning the whole shape expression. `field`, `value_type` (a `WireShape`, or null when the shape failed to parse). Encloses the leaves below; carries the full form (record / reference / inline-or-reference / def-ref / compound / list / pinned / primitive / enum) so a consumer differentiates or collapses at will.
  - `type-ref` — a navigable type-def name. `name`. A field-shape name (record / reference / inline base, compound operand, def-ref bound) or a `sealed:` branch. Broken-ness rides the diagnostics read, never a token flag.
  - `shape-builtin` — a built-in shape keyword. `name`. A primitive, `file`, `any`, or `type`.
  - `enum-member` — an inline enum literal. `value`.
- a wikilink-valued field is a `wikilink-*` token, never a `field-value`.
- a note (no `type:` claim) carries only the body-surface kinds: `wikilink-*`, `block-id`, `anchor`.
- a type-def file carries only the shape kinds (`field-shape` / `type-ref` / `shape-builtin` / `enum-member` / `type-claim`), no value-layer or body-surface kinds.
- the read colors by type, so a declared primitive and an inferred primitive are wire-identical; whether a field is declared is a schema question, on `diagnostics` / `type`.
- tokens reflect the last build (disk). A consumer refreshes via the `changes` subscription; there is no dedicated channel.
- a quoted field shape (e.g. `r: "decision*"`) emits the `field-shape` container but not its leaves; the normalized scalar's offsets don't map onto the source, so the per-name leaves are dropped rather than mis-placed.

**top_level_dirs.**
- args: optional `repo` (one member; absent spans every mounted member) and optional `scope` (`own` default / `all`). An unknown arg is an `error` frame, never silently ignored: a typo'd `scope` must not quietly return a different-scoped set. See "Scope defaults" above.
- result: `{ top_level_dirs: [...] }` — the top-level directories of every mounted member in scope, each holding at least one catalogued file, hidden excluded.
- each: `repo` (the member that owns the directory), `name`, `path`, `member` (the declared name of the mounted member ROOTED at this directory, else null).
- a member may sit physically INSIDE another repo. Its folder then genuinely is a directory of the containing repo, so it is reported for BOTH: once as the container's own top-level directory, and again as the member's own dirs under the member's tag. The overlap is honest, not double-counting to filter away — `member` is what makes it legible, and a consumer collapses or annotates the duplicate subtree with it. Derivable by joining `path` against every `members[].root`, but that pushes a path-equality comparison onto every consumer for a fact the engine knows for free; it carries the member NAME rather than a bare flag so the loop closes without a second read, matching `instances_of.member`. Non-null only for a DECLARED member: the content walk is bounded at an undeclared nested repo's marker, so such a directory holds no catalogued file and never appears here at all.
- **`schema_version` 17**: renamed from `top_level_graphs` and RESCOPED. The old read walked only the ENTRY root, which answers approximately nothing in the expected topology — a thin entry repo carrying `workspace.yaml` with the content in `edit` members — and it was the sole entry-scoped field in an otherwise workspace-scoped `overview`. Entries also gain `repo`, and `folder_path` is now `path`. "Graph" was a misnomer: these are folders, and nothing about the typed graph is consulted.
- catalog-derived, so a directory holding no catalogued file does not appear — the same rule as `dir_entries`.

**members.**
- args: optional `repo`, narrowing to that one member; absent lists all. An unknown arg is an `error` frame. `members` honors `repo` but NOT `scope` — it is the TOPOLOGY field, and `members.editable` is how a consumer sees which members `own` selected, so filtering by class would hide the dependency set from the one field whose job is to report it. **`schema_version` 17**: `repo` is new, so `overview({repo}).members` is now reproducible by one `members({repo})` call, closing the mirror hole for this field.
- result: `{ members }` — the workspace's declared members, name-sorted.
- each: `repo` (declared name), `root` (absolute root on this machine), `scattered` (true when the root sits outside the workspace root, reached by absolute path; false for a subdir of it), `editable` (true for an editable authoring surface, false for a consumed member), `local` (true for a live local working tree, false for a read-only cache snapshot), `role` (`entry` / `edit` / `discover` / `dep`), `disabled` (true when the member is declared in the workspace's `disabled:` overlay, intentionally not mounted; false for a mounted member), `git` (which working tree covers the member).
- `git` is `{ tracked, root }`. `tracked` is true when SOME git working tree covers the member; `root` is that tree's absolute root, `null` when `tracked` is false. The covering tree is often an ANCESTOR of the member's own `root`, not equal to it: a monorepo holding twelve members in one tree reports all twelve `tracked: true` with the SAME `git.root`, and that shared value is what says they commit together. Served because a consumer cannot derive it — checking `<member>/.git` reports every nested member untracked while the engine refactors across them happily. It is the same predicate the write path groups by, so the badge a consumer renders and the tree a mutation commits into cannot disagree. `tracked: false` is the genuine non-git case: writes land but never commit, and a structural refactor touching that member is refused because it could not be rolled back. Additive, no `schema_version` bump. See [[spec - git write path - commit-per-mutation as a local saga over the workspace's materialized repos]].
- the member shape carries two orthogonal axes plus the raw role. `editable` is the ROLE axis: the entry and an `edit` member are editable authoring surfaces, a `dep` or `discover` member is consumed, independent of where it resolved on disk — so a co-present dep is `editable: false`, and an `edit` member present only in the cache stays `editable: true`. `local` is the LOCATION axis: a live working tree (a co-present sibling or a registry path) versus a read-only cache snapshot (under the device package cache). The two are independent: a co-present dep is `editable: false, local: true` (consumed but writable in place, also carrying `dependency-path-overridden`), an `edit` member only in the cache is `editable: true, local: false` (`edit-member-read-only`). `role` is the full four-way signal `editable` derives from, and distinguishes a `discover` mount from a plain `dep`. The consumer applies policy: hide consumed members with `!editable`, scope physical writes with `editable && local`. **`schema_version` 16**: drops the former `primary` boolean (its role meaning is now `editable`); `editable` was location-derived through schema 15.
- `disabled` is the `disabled:` overlay axis: a member declared in the workspace's `disabled:` list is intentionally NOT mounted — it contributes no files, types, or vocabulary, and fires NO diagnostic. Distinct from the role-keyed `*-member-unmounted` codes, which mean declared-but-cannot-be-found; disabled is found-but-switched-off-on-purpose. A disabled member carries an empty `root`, `local: false`, and `disabled: true`, with its DECLARED `edit` / `discover` `role`; a consumer renders it greyed with a toggle and flips it by editing the manifest's `disabled:` list. The hardwired `au.engine.workspace` def gains an optional `disabled?: String[]` overlay (a name may sit in `edit:` / `discover:` and `disabled:` at once, disabling never rewrites the role). A `disabled:` entry naming no declared member is the `disabled-member-not-declared` warning. Additive, no `schema_version` bump.
- surfaces the member topology the engine holds internally, so a consumer addresses a scattered member's files without reconstructing it.

**overview.**
- args: optional `repo` and optional `scope` (`own` default / `all`). See "Scope defaults" above.
- result: `{ overview: {...} }`, the up-front orientation map, one cheap read a consumer loads first, then drills through the other reads.
- `repo` and `scope`, the RESOLVED args echoed back, so a consumer drilling from a field into its own read passes them verbatim instead of re-deriving a default it cannot see. (`subtypes.base` echoes the same way.)
- fields, each ONE CALL TO ITS OWN READ with these same args:
  - `members`: the `members` read's array directly (the `MemberView` element shape, NOT the `{ members: [...] }` wrapper), so a consumer reads `overview.members`, not `.members.members`.
  - `top_level_dirs`: the `top_level_dirs` read's array (`repo`, `name`, `path`, `member`).
  - `type_counts`: the `type_counts` read's object (`total`, `by_repo`).
  - `diagnostic_counts`: the `diagnostic_counts` read's object (`total`, `by_severity`, `by_code`).
  - `hubs`: the `hubs` read's array, bounded to the engine's top-N.
  - `graph_shape`: the `graph_shape` read's object (the scalar whole-graph summary), with `orphan_paths` off — the unbounded path list is a drill, not a summary card.
- **`members` honors `repo` but NOT `scope`.** It is the TOPOLOGY field; the other four report what is IN the content, and `scope` filters content. Filtering `members` would hide the dependency set from the one field whose job is to report it, and `members.editable` is exactly how a consumer sees which repos `own` selected. A present `repo` does narrow it — that is a selection, not a class filter.
- **The mirror property**: every field is reproducible by one call to its own read, with the echoed `repo` / `scope`. `top_level_dirs`, `diagnostic_counts`, and `hubs` share `overview`'s `own` default, so an argless drill-down into those agrees literally. `type_counts` is the one exception (it keeps `all`, see "Scope defaults"), which is exactly why the echo exists. `members` honors `repo` but not `scope`, so it mirrors via its own `repo` arg (the `members` read gained `repo` in schema 17 to close that gap). Taking this property seriously is what surfaced that `diagnostic_counts` could not express repo scoping at all, that `hubs` had no read, and that `members` could not be scoped by `repo` — all three added rather than the property being weakened.
- **`schema_version` 17**: BREAKING beyond a shape change — `type_counts`, `diagnostic_counts`, `hubs`, and `top_level_dirs` all return DIFFERENT VALUES for the same argless call, since it is now `own`-scoped. Pass `scope: "all"` for the previous behaviour.
- computed over the graph, never hand-authored. Read-only, refreshed through the `changes` subscription.

**hubs.**
- args: optional `repo`, optional `scope` (`own` default / `all`, see "Scope defaults"), optional `limit` (absent = the engine's top-N, the same bound `overview.hubs` carries) and `offset`.
- result: `{ hubs: [...] }` — the most-referenced files over the typed reference graph, the "start here" signal a generic search cannot give.
- each: `path`, `repo` (owner, or null), `kind` (`instance` | `type-def` | `note` | `asset`), `refs_structural`, `refs_navigational`, `refs_total`.
  - `asset` is a catalogued file the build never read, distinct from `note` (a markdown file read and found to carry no `type:`). An asset ranks like any node: the walker catalogues it precisely so `file*` resolves against it, so a referenced PDF or image genuinely appears here. One derivation serves this, `files`, and a `neighborhood` node's `file_kind`, so the vocabulary cannot drift per read.
  - `refs_structural` counts inbound edges filling a typed slot; `refs_navigational` the prose links. The split is the neutral fact a consumer re-ranks by, there is no opaque score.
  - ranked `refs_structural` desc, then `refs_total` desc, then path. A file with no inbound edges never appears.
- **scoping happens BEFORE ranking and truncation.** A dependency larger than the top-N bound would otherwise fill every slot and crowd the user's own content out of the ranking entirely, and a client-side filter over the truncated page cannot recover it — it would return fewer than `limit` own hubs while own hubs existed.
- **`schema_version` 17**: NEW. Promoted out of `overview`, whose `hubs` field had no read to mirror since it shipped; `overview`'s mirror property was therefore never fully true until now.

**graph_shape.**
- args: optional `repo`, optional `scope` (`own` default / `all`, see "Scope defaults"), optional `orphan_paths` (bool, default false).
- result: `{ graph_shape: {...} }` — the scalar whole-graph summary folded from the catalog + backlink index, the "your knowledge base is 14 disconnected components" signal. An orphan is a READ here, never a diagnostic: growth is never rejected, and a not-yet-linked file is the forge signal `reference-target-missing` is deliberately soft about.
- `repo` / `scope`, the RESOLVED args echoed back, same as `overview`.
- the node universe is the CONTENT catalog: type-defs, instances, notes, assets. Engine-schema files (`.arsumbris/repo.yaml`, the workspace manifest, `repo.lock`) are substrate config, never a node, an orphan, or a component. Edges are the resolved backlink index induced on in-scope endpoints (both endpoints in scope).
- fields:
  - `node_count`, `edge_count`, and the `edges_structural` / `edges_navigational` split (the `slot != null` partition `hubs` uses), so density per basis is derivable.
  - `components`: the weakly-connected-component count over the undirected COMBINED graph (structural and navigational joined — any link joins two files into one island). `largest_component`: the node count of the biggest, distinguishing "14 tiny islands" from "one web plus 13 strays".
  - `orphans`: `{ no_inbound, isolated, paths? }`. `no_inbound` counts files nothing points at (the useful sense); `isolated` counts files with no inbound AND no outbound (the strict subset, a singleton component). `paths` is present only with `orphan_paths: true`, each `{ path, kind }` tagged `no_inbound` or `isolated`, sorted by path. Counts are bounded; the path list is not.
  - `degree`: `{ in: [...], out: [...] }`, each a histogram of `{ degree, count }` sorted by degree — `count` nodes carry exactly `degree` edges on that axis. The distribution SHAPE is the signal; `hubs` serves the per-node top.
  - `density`: `edge_count / node_count`, a convenience over the raw counts.
- raw facts, NO thresholds — the graph-engineering paper's numbers come from a 22-node corpus and are not evidence, so the consumer applies policy (the same fact-not-policy split `hubs` took).
- computed over the graph, never hand-authored. Read-only, refreshed through `changes`. Mirrored by `overview.graph_shape`. **Additive, no `schema_version` bump.** See [[spec - whole-graph reads - a scalar shape summary and a full node-edge payload folded from the backlink index]].

**link_graph.**
- args: optional `repo`, optional `scope` (`own` default / `all`, see "Scope defaults"). Richer server-side filters (edge kinds, type filters, path prefix) are a deferred follow-on; an unknown arg is an `error` frame today.
- result: `{ link_graph: { repo, scope, nodes, edges } }` — the full node+edge payload a whole-graph (force-directed) visualization lays out. The un-ranked, un-truncated sibling of `hubs`, and the global-unseeded complement of the seeded `neighborhood` walk.
- `repo` / `scope`, the RESOLVED args echoed back.
- `nodes`: every in-scope CONTENT file (the same node universe as `graph_shape` — engine-schema files excluded), isolated ones INCLUDED (a graph view draws floating orphans). Each `{ path, repo, kind, refs_structural, refs_total }`:
  - `kind`: `instance` / `type-def` / `note` / `asset`, the shared `hubs` / `files` / `neighborhood` vocabulary.
  - `refs_structural` / `refs_total`: the inbound hub counts, for free node-sizing (`refs_navigational` is the difference). Over the IN-SCOPE inbound edges, so a node's `refs_total` equals its inbound-edge count in this same payload.
- `edges`: every resolved reference edge with both endpoints in scope. Each `{ from, to, kind, surface }`:
  - `kind`: the coarse edge kind (`field` / `contributing` / `navigational`), `surface`: `frontmatter` / `body` — the same projection `references_in` serves.
  - resolved edges only; a dangling link is a diagnostic (`reference-target-missing`), never an edge here. Edge multiplicity is preserved (two links to one target are two edges), so `refs_total` and the edge rows agree.
- `nodes` sorted by `path`, `edges` sorted by `(from, to, kind, surface)`, so the payload is deterministic (and the `link_graph` subscription diff is stable).
- computed over the graph, never hand-authored. Read-only; a consumer keeps it fresh via the `link_graph` SUBSCRIPTION (below), which streams node/edge deltas rather than re-shipping the payload. NOT an `overview` field — it is an unbounded payload, the drill target, like the full `diagnostics` list is not folded into `diagnostic_counts`. **Additive, no `schema_version` bump.** See [[spec - whole-graph reads - a scalar shape summary and a full node-edge payload folded from the backlink index]].

**type_graph.**
- args: optional `repo`, optional `scope` (`own` default / `all`, see "Scope defaults"), optional `edges`. An unknown arg, or an unknown `edges` value, is an `error` frame.
- result: `{ type_graph: { repo, scope, nodes, edges } }` — the SCHEMA graph as a node+edge payload, the type-side sibling of `link_graph`. Drawn beside `link_graph` and merged by `path`; the type graph is a distinct held structure from the reference (wikilink) graph `link_graph` folds.
- `repo` / `scope`, the RESOLVED args echoed back.
- `edges`: which edge classes to include, an array of `subtype` / `field-type` / `instance-of` / `meta`. ABSENT is the type-to-type backbone `["subtype","field-type"]`; `instance-of` and `meta` are opt-in (`instance-of` pulls every claiming instance in as a node).
- `nodes`: the in-scope type-defs, plus a claiming instance when `instance-of` is requested. Each `{ path, repo, kind }` — `kind` is `type-def` or `instance`. Node identity is `path`, so the payload merges with `link_graph` by path. NO ref counts here: the reference-graph counts stay on `link_graph` (its own concern); node sizing derives from the edges (subtype / instance-of in-degree).
- `edges`: the resolved type-graph edges induced on in-scope endpoints. Each `{ from, to, relation, count }`:
  - `relation`: `subtype` (type-def → parent), `field-type` (type-def → a type its field shape references), `instance-of` (file → claimed type-def), `meta` (type-def → its meta type). Direction is source → target (child / referrer / instance as `from`).
  - `count`: the deduped multiplicity — a `field-type` pair folds into ONE edge weighted by the number of field references to the target (an edge weight; a field whose shape names the target twice counts twice). The other relations are single by construction, so `count` is 1.
  - induced-subgraph rule, like `link_graph`: an edge counts only when BOTH endpoints are in scope, so a cross-repo edge to an out-of-scope peer type drops under `scope: own`.
- `nodes` sorted by `path`, `edges` sorted by `(from, to, relation)`, deterministic (and the `type_graph` subscription diff is stable).
- computed over the type graph, never hand-authored. Read-only; a consumer keeps it fresh via the `type_graph` SUBSCRIPTION (below). **Additive, no `schema_version` bump** (rides the schema 23 bump the channel renames carry). See [[spec - whole-graph reads - a scalar shape summary and a full node-edge payload folded from the backlink index]].

**resolve_member.**
- args: `path`.
- result: `{ resolve_member: { repo, root, editable, local, role } | null }` — the declared member that owns the path, by the same deepest-ancestor rule reads and writes use; null when the path lies under no declared member.
- `editable` / `local` / `role` carry the same meaning as on `members`, so the cage scopes a write by the member it lands in (`editable && local`). **`schema_version` 16**: drops the former `primary` boolean, adds `local` / `role` (matching `members`).
- given a path, route to the owning member without re-deriving member roots client-side.

**ignores.**
- args: `repo` (optional; scope to one member, unknown → empty `members`), `resolve` (optional bool, default false).
- result: `{ ignores: [...] }` — one entry per member (or the one named), the file-scope rules read out-of-band (no walk). **`schema_version` 17**: the payload key was `members`, which collided in shape-name with the `members` read while carrying a different element type.
- each: `root` (absolute member root), `repo` (declared name), `patterns` (the editable `.auignore` lines, verbatim, comments and blanks included for a faithful `set_ignores` round-trip; empty when the file is absent), `default_excludes` (`node_modules`, `target`, seeded and overridable), `floor` (`.git`, `.arsumbris`, unconditional and NOT editable).
- with `resolve: true`, each entry also carries `resolved: { ignored_dirs, ignored_files }` — the boundary-level effect at one bounded extra walk. Boundaries, NOT contents: a pruned directory is ONE `ignored_dirs` entry and its contents are never enumerated (a pruned `node_modules` is one entry); an individually-excluded file is one `ignored_files` entry. The floor is not repeated here (it is the `floor` field). Omitted without `resolve`.
- pairs with the `set_ignores` mutation, which edits `patterns`. Additive, no `schema_version` bump.

**device_config.**
- args: none.
- result: `{ device_config: { repos, workspaces } }` — the two per-user device-global engine-schema files, `~/.arsumbris/au-engine/config/repos.yaml` and `~/.arsumbris/au-engine/config/workspaces.yaml`.
- each entry is `{ path, exists, content, diagnostics }`, or null when its path cannot be resolved (no device root: `$HOME` unset).
  - `path`: the resolved absolute path, present even when the file is absent, so a consumer knows where to author.
  - `exists`: true when the file was read; false is a legitimate not-yet-authored state, with null `content` and no `diagnostics`.
  - `content`: the raw file text when present and UTF-8; null when absent or not UTF-8 (the not-UTF-8 case still carries a diagnostic).
  - `diagnostics`: the field-shape verdict against the file's hardwired def (`au.engine.repos` / `au.engine.workspaces`), the same shape and codes as the `diagnostics` read, with spans into THIS real file.
- these files sit OUTSIDE every knowledge base, so they are not knowledge-base nodes and their diagnostics land HERE, not on the workspace `diagnostics` read. Field-shape only: no references, no closure or reference index to walk.
- the paired writer for `repos.yaml` is the `register` mutation; `workspaces.yaml` is user-authored. **`schema_version` 12.**

**config.**
- args: `{ scope, consumer, file, type, root? }` — one CONSUMER config file under the scoped-config channel.
  - `scope`: `"machine"` (`~/.arsumbris/<consumer>/config/<file>`) or `"repo"` (`<repo>/.arsumbris/<consumer>/config/<file>`).
  - `consumer`: the owner segment, a namespacing convention (`host-app`, `agent-tools`). Path-safe, and not the engine's own `au-engine`.
  - `file`: the config filename under `<consumer>/config/`.
  - `type`: the DECLARED type the value is field-shape-validated against — the floor. A file's own written `type:` wins and may subtype it (mix in more); an absent one stamps this declared type and carries a `config-type-unwritten` drift. The paired `set_config` write injects `type:`, so a governed write self-describes.
  - `root` (repo scope only): the member root whose `.arsumbris/` holds the file, a declared member root (as `members` surfaces); absent defaults to the served entry. Ignored at machine scope.
- result: `{ config: { path, exists, content, diagnostics } }` — the same per-file view `device_config` returns, for one file.
  - `path`: the resolved absolute path, present even when absent; null only when the scope has no resolvable base (machine scope, `$HOME` unset).
  - `exists` / `content`: as `device_config`.
  - `diagnostics`: the field-shape verdict against the file's own `type:` (else the declared `type` stamped) resolved in the scope's graph (repo → owning member, machine → served entry, `foo::repo` → the peer). A single `config-type-unresolved` advisory (warning) when the type does not resolve — the value is stored-as-is, never the hard `unknown-type-claim`. A `config-type-unwritten` drift when the file carries no written `type:`.
- read OUT-OF-BAND by the verb, the file is NOT a walked node: no `instances_of`, no candidate scan, no reference-liveness check on wikilink values. The consumer owns merge, resolution, and drift across scopes; the engine never merges.
- an unknown repo-scope `root`, or a path-unsafe / reserved `consumer` / `file`, is an `error`-kind response, nothing read.
- the paired writer is the `set_config` mutation. A NEW read (a new `read` verb and a new response `type`), additive, so NO `schema_version` bump.

**lifecycle.**
- args: none.
- result: `{ lifecycle: { engine, ref } }`, the engine and ref lifecycle states.
- always returns, carrying the frame's `ready` and `version`, the readiness probe.
- **`schema_version` 17**: renamed from `ready`, which meant three things at once — the frame's can-I-answer flag, this read, and a state VALUE. The frame keeps the flag; the read and its subscription channel take `lifecycle`.


## Subscriptions

A subscription is tied to its connection.
- it delivers an ack, then an initial value when the channel has one, then change events.
- it dies when the connection closes; no persistent subscription, stored offset, or cross-connection resume.
- a consumer that reconnects re-subscribes and re-receives initial values, then listens from that point.

The daemon multiplexes many subscriptions on one connection.
Every `ack`, `initial_value`, and `change_event` carries a `subscription_id`, so the consumer routes each frame to the subscription that asked for it.
The id is a per-connection integer, assigned in the ack.

### ack

Acknowledges a `subscribe`, delivered immediately.

- `type`: `"ack"`.
- `subscription_id`: the per-connection id for this subscription.
- `channel`: the channel name, echoed.
- `accepted`: always `true`. an ack means the subscribe was accepted.
- `id`: the request's correlation id, echoed when it carried one.

An unknown channel or malformed arguments is rejected as an `error` frame with `for: "subscribe"`, not an ack; no `subscription_id` is assigned and no further frames follow.

### initial_value

The channel's current state, delivered once after the ack.
- immediate when the state already exists, deferred until it does otherwise, e.g. subscribing before the first build completes.
- channels with no initial state skip it; only `changes` has none.

- `type`: `"initial_value"`.
- `subscription_id`.
- `at_version`: the ref version the state was observed at.
- `result`: the state, in the shape of the channel's paired read.

### change_event

A notification that the channel changed. Notification-only, it never carries the new state.

- `type`: `"change_event"`.
- `subscription_id`.
- `kind`: the kind of change, per channel below.
- `at_version`: the ref version after the change.
- `scope_hint`: what changed, per channel.
  - `changes`: `{ "scope": "files", "added": [...], "removed": [...], "modified": [...] }`, the net file delta.
  - `files`: `{ "scope": "files", "added": [...], "removed": [...] }`, the path-set delta.
  - `types`: `{ "scope": "types", "added": [...], "removed": [...], "changed": [...] }`, each an identity handle `{ name, repo, hash }` (not a bare name), so a consumer knows WHICH same-named cross-repo type changed.
  - `diagnostics`: `{ "scope": "files", "files": [...] }`, the exact in-scope files whose diagnostic set changed.
  - `link_graph`: `{ "scope": "link_graph", "nodes_added": [...], "nodes_removed": [...], "edges_added": [...], "edges_removed": [...] }`, the node/edge delta — `nodes_added` full node records (a re-referenced file re-emits with new ref counts), `nodes_removed` paths, the edge lists full edge records. UNLIKE the other channels, a consumer applies this delta directly rather than re-reading, since the payload is large.
  - `type_graph`: `{ "scope": "type_graph", "nodes_added": [...], "nodes_removed": [...], "edges_added": [...], "edges_removed": [...] }`, the schema-graph node/edge delta — same shape as `link_graph`, `nodes_removed` paths, the rest full records; an edge re-emits when its `count` changes.
  - `lifecycle`: `{ "scope": "knowledge-base" }` (**`schema_version` 21**, the value was `vault`).

The consumer re-queries the paired read to get new state.
The event channel stays low-bandwidth, a change event is a few hundred bytes whether one field changed or the whole knowledge base rebuilt.

Precise hints don't need incremental recompute.
- each channel diffs its own projection across the full rebuild, cheaply, and reports the delta.
- a per-subscription task holds the state it last delivered, so coalesced rebuilds yield one net delta covering everything since the subscriber's last event.
- a consumer re-reads only what the hint names instead of the whole knowledge base.

Field- and entity-granularity deltas (which frontmatter key changed, which record) stay deferred.
- deferral kind: uncertain need, no consumer pulls on them yet.

### The subscription catalog

Closed, like the read catalog. Each channel names its initial value and what fires an event.

**lifecycle.**
- args: none.
- initial: `{ lifecycle: { engine, ref } }`, the `lifecycle` read's result shape.
- event `lifecycle-changed`: the ref's Deriving-to-Ready transition.
- **`schema_version` 17**: renamed from `ready` with the read; the event kind renamed from `ready-changed`.

**types.**
- args: none.
- initial: the workspace-wide type-def introspection, the no-`repo` `types` read's result shape (owner-deduped, owner-annotated).
- event `types-changed`: a rebuild after which any type-def's wire introspection differs.
  - the `scope_hint` names the added, removed, and changed types, each an identity handle `{ name, repo, hash }`. The diff keys on `(name, repo)`, so two mounted repos owning a same-named type are distinct, a change to either fires and names its own identity.
  - a type-def file edit that leaves every introspection unchanged does not fire.
- **`schema_version` 23**: the channel was `type-graph`, the event `type-graph-changed`. The subscribe name now matches its `types` read (subscribe channels are snake_case, like read names); event kinds stay kebab-case. The `type-graph` name is reassigned to the new `type_graph` drawable subscription (below).

**files.**
- args: none.
- initial: the catalogued files, an array of `{ path }`, absolute, sorted.
- event `files-changed`: a rebuild whose set of catalogued file paths differs.
  - the `scope_hint` carries the added and removed paths.
- **`schema_version` 21**: the channel was `vault.files`, the event `vault-files-changed`. It was the only channel name carrying a dot; it now matches its bare-noun siblings.

**changes.**
- args: none.
- initial: none.
- event `knowledge-base-changed`: a rebuild whose net file delta is non-empty (**`schema_version` 21**, the kind was `vault-changed`).
  - the `scope_hint` carries the added, removed, and modified paths.
  - modified means the content hash moved; asset files are fingerprinted by presence only, so their edits never appear here (they cannot alter the analysis).
  - rebuilds a slow subscriber skipped coalesce into one event with the net delta since its last event; a delta that cancels out entirely does not fire.
- external edits coalesce: the watcher rebuilds after 150ms of quiet, so an edit burst yields one rebuild and one event. Consumers don't need their own debounce.
- source classification of the edit, authorized versus unauthorized, is deferred; every edit is external today.
- LIMITATION — a runtime-added SCATTERED member is not watched until restart. The watched member set is computed once when the daemon starts. If the resolved member set gains a member OUTSIDE the entry tree after startup (a `register`, a manifest edit, or a new `.arsumbris/repo.yaml`), that member's current content is ingested on the triggering rebuild, but its subtree is not watched, so later edits inside it fire no `knowledge-base-changed` / `files-changed` / `diagnostics-changed` event until the daemon restarts. Co-present in-tree members are unaffected. A consumer that adds a scattered member at runtime (e.g. via `register`) should restart the daemon for that member's live edits to surface. Tracked engine-side; re-arming the watch on a member-set change is a planned improvement.

**diagnostics.**
- args: the same filters as the `diagnostics` read (`path`, `path_prefix`, `severity`, `code`, `repo`, `scope`), all optional, composing (AND). No args is whole-knowledge-base. Unknown args are an `error` frame. The channel shares the read's filter struct, so the two can never diverge; `repo` / `scope` are new in **`schema_version` 17**.
- initial: the diagnostics in scope, an array in the `diagnostics` read's per-diagnostic shape.
- event `diagnostics-changed`: a rebuild after which the diagnostic set in scope differs.
  - the `scope_hint` names the exact in-scope files that changed, so a consumer re-reads only those.
- the whole-knowledge-base form pairs with the `diagnostics` read for a Problems panel: subscribe once, wake on the changed files.
- a consumer wanting specific files instead opens one subscription per file; the daemon multiplexes them.

**link_graph.**
- args: optional `repo`, optional `scope` (`own` default / `all`), the same args as the `link_graph` read. Unknown args are an `error` frame.
- initial: the full `link_graph` payload (`{ repo, scope, nodes, edges }`) at the subscribe version.
- event `link-graph-changed`: a rebuild whose net node/edge change in scope is non-empty.
  - the `scope_hint` carries `nodes_added` / `nodes_removed` / `edges_added` / `edges_removed`. A node re-emits in `nodes_added` when its record changes (kind, repo, or ref counts), not only on first appearance; edges diff as a multiset, so preserved edge multiplicity carries through.
  - UNLIKE the read-then-re-read channels, this streams the delta so an interactive force-directed layout patches in place. A rebuild that leaves the in-scope graph unchanged does not fire.
- pairs with the `link_graph` read: a consumer reads once (or takes the subscription's initial value) then applies deltas.
- **`schema_version` 23**: the channel was `link-graph`, matching its `link_graph` read now (subscribe channels are snake_case); the event kind `link-graph-changed` stays kebab-case, as do all event kinds.

**type_graph.**
- args: optional `repo`, optional `scope` (`own` default / `all`), optional `edges`, the same args as the `type_graph` read. An unknown arg (or `edges` value) is an `error` frame.
- initial: the full `type_graph` payload (`{ repo, scope, nodes, edges }`) at the subscribe version.
- event `type-graph-changed`: a rebuild whose net node/edge change in scope is non-empty. (This kebab kind is reassigned in **`schema_version` 23** from the old introspection stream, now the `types` channel with `types-changed`, to this drawable stream.)
  - the `scope_hint` carries `nodes_added` / `nodes_removed` / `edges_added` / `edges_removed`. A node re-emits in `nodes_added` when its record changes; an edge re-emits when its `count` changes (edges are unique per `(from, to, relation)`).
  - EDGE-DIFF SHAPE DIFFERS from `link_graph`, despite the sibling framing: `type_graph` edges are unique per `(from, to, relation)` and UPSERT — a `count` change re-emits the edge in `edges_added` ONLY, never `edges_removed`. `link_graph` edges carry no weight and diff as a MULTISET (pure add / remove). So an SDK keys `type_graph` `edges_added` by `(from, to, relation)` and REPLACES, rather than appending as it may for `link_graph`.
  - the type-side sibling of the `link_graph` subscription; a rebuild that leaves the in-scope type graph unchanged does not fire.
- pairs with the `type_graph` read: read (or take the initial value) once, then apply deltas. **Additive within the schema 23 bump.**

**recent_commits.**
- a NEW channel, additive (a new subscribe name and event kind), so NO `schema_version` bump. The live half of the git ACTIVITY view; pairs with the `recent_commits` read.
- args: the same `members` / `limit` / `since` as the read. Unknown args are an `error` frame.
- initial: the seed page, the `recent_commits` read's result shape (an array of rows) at the subscribe version.
- event `commits-appeared`: a newly-appeared commit in a watched tree.
  - the `scope_hint` carries `commits`, an array of the new rows (the read's row shape). APPEND-ONLY: the server emits new commits, never removals; the client owns its bounded window and trims its own tail. A commit is emitted once, keyed by oid, so an overlap between the seed and the first event is an idempotent duplicate the client dedups.
  - a new commit inserts by committer timestamp, not always at the head (clock skew across trees can date it below an existing row).
- LIVENESS IS THE REFLOG WATCHER, NOT THE VERSION SIGNAL. `recent_commits` is a git-state projection: the daemon arms an on-demand reflog watcher per watched tree while the subscription is open (ref-counted, torn down on close), and re-logs the tree that moved. So a commit from a terminal (out-of-band, advancing no knowledge-base version) surfaces live, exactly what a version-driven channel would miss. A content edit that commits nothing fires nothing here.
- OUT OF SCOPE — history rewrite. A `reset` / `amend` / `rebase` moves HEAD non-monotonically and can invalidate emitted rows, but it violates the workspace's append-only invariant (which the engine depends on and cannot enforce), so it is the violator's concern, not a delta this channel models.
- LIMITATION — a member (and its tree) mounted at runtime while a subscription is open is not watched until the subscription reopens (the tree set is resolved once, at subscribe). Tracked engine-side; re-arming on a member-set change is a planned improvement. The rare case; the common workspace is stable across a session.
- `at_version` on an event is the held knowledge-base version at emit, a monotonic stamp, NOT a git coherence cursor (the commit is off the version axis).


## The mutation catalog

Closed, per [[spec - mutation channel v1 - a closed primitive catalog through one mediated path]].
The one mediated write path; no consumer writes behind the engine's back.

The pipeline is synchronous: guard → write → commit → rebuild → respond.
The commit step runs when the owning repo is a git working tree; a non-git repo writes without committing.
The response returns after the rebuild, so the consumer's next read is usually already current.
The disk write is canonical; the rebuild is best-effort and can lag, see `reflected` below.

Mutations are advisory like everything else:
a mutation never rejects because the result has validation errors — the response carries the touched file's fresh diagnostics.
Rejection is for malformed requests only: bad args, path escape, hash mismatch, or an uncommitted working-tree change at a touched path (the clean-at-HEAD precondition, git repos only).

A successful mutation answers in the read-response envelope.
- `version`: the post-mutation knowledge base version — the originator's echo-suppression correlation; it skips change events carrying this `at_version`.
- `result`: `{ path, hash, diagnostics, reflected, commit, commits }`, plus primitive-specific fields.
- `hash` is the held knowledge base's content hash for the file, ready as the next `expected_hash`. `resolve_target` serves the same hash for the first read.
- a mutation is a local saga over its touched repos: it commits once per committing member, so a multi-repo mutation (a rename rewriting referrers across repos) makes N commits, each on its repo's current branch carrying a shared `Mutation-Id` trailer. The result carries an anchor and a provenance field, which answer different questions.
- `commit` (the anchor — "what do I pin `result.path` at?"): HEAD of `result.path`'s OWN repo, the pin anchor for the file the response is about. `null` ONLY off-git (`result.path` not under a git working tree). After a committing mutation it is the new sha; after an idempotent no-op it is the unchanged HEAD — still a valid anchor, NOT null. Present-but-nullable, symmetric with the content read's `commit` (a read never commits, so its `commit` is the same HEAD anchor). NOT first-of-N: in a multi-repo saga it follows `result.path`'s repo, never a different repo's sha.
- `commits` (the provenance — "what did THIS mutation commit?"): every committing repo as `{ [repo]: sha }`, keyed by repo name. Nothing dropped. `{}` when nothing committed (every touched member off-git, or a no-op that wrote nothing). A member written but not committed (an off-git member in a mixed saga) has no entry — no sha exists for it. Mutate-only; a read has no provenance. `result.path`'s own repo is always among the committers when the saga commits anything (the mutation's primary effect is on `result.path`), so a non-empty `commits` means `commit` is the sha that just changed `result.path`; an empty `commits` means `commit` is a pre-existing HEAD.
- a touched path with an uncommitted change rejects first (clean-at-HEAD): the engine is the one writer, so commit or discard the change before mutating.
- `reflected`: whether the held knowledge base already reflects this write. `true` in the common case (the rebuild committed). `false` means the disk write landed but the rebuild has not caught up, so `hash` and `diagnostics` predate the write — await a later `version` (the watcher re-drives the rebuild), do NOT re-issue the mutation (its `expected_hash` would now conflict). An idempotent no-op (`assign_block_id` on an already-addressed record) is always `true`, nothing was written to lag.

A rejection is an `error` frame; an optional `detail` object carries machine-usable context.
Nothing was written.

Path rules: workspace-relative or absolute; a write reaches any declared workspace member (scattered or subdir) the same way a read does; the escape and `.arsumbris/` guards apply per member; a path under no declared member rejects (the real escape).

Editability gate: a mutation may author only an EDITABLE member (the entry or an `edit` member); a consumed (`discover` / `dep`) member is not the engine's to write ([[spec - workspace as a folder-repo - an optional workspace.yaml composes edit and discover members]]).
- a write whose path lands in a consumed member is an `error` frame naming the member and the promote-to-`edit` fix. `rename_type` over a consumed member's type-def rejects the same way (its def path is graph-derived, not a path arg), as do the config-channel writes `set_config` (repo scope) and `set_ignores` (they resolve a member root directly, and a consumed member's config / `.auignore` is not the engine's to author).
- a structural refactor (`rename` / `promote` / `inline` / `rename_block_id` / `rename_type`) additionally rejects when a referrer it would rewrite lives in a consumed member — it may not author files it does not own. The `error` frame carries `detail.blocking_consumed_referrers`, a `{ <member>: [<file>, ...] }` map, so a consumer offers the choice: promote the member to `edit`, or (a tracked future flag) opt in to skip and leave an advisory `dangling-reference`. A commit-pinned, byte-identical referrer never triggers it.
- additive, no `schema_version` bump: a new reject reason on existing ops, over the already-optional error `detail`.

Stamps rider (`schema_version` 24): every write verb EXCEPT `delete_file` accepts an optional `stamps`, a LIST of caller-supplied records the engine idempotently ensures in named frontmatter list-fields of the file the write touches, all folded into the write's OWN commit ([[spec - stamp injection - a write rider idempotently ensures a frontmatter record folded into the write's commit]]). So many stampers coexist on one write without a second mutation.
- shape: `stamps: [{ field, record, match_on? }, ...]`. `field` is the frontmatter sequence key; `record` is an arbitrary structured value; `match_on` is an optional dedup predicate, a map of element-slot to value. An absent or empty list is exactly no stamp; a length-1 list is exactly the former single stamp.
- idempotent ensure, per entry: with `match_on`, if some existing element of `field` carries every pair, that stamp is a NO-OP; otherwise `record` is appended. Without `match_on`, always appended. A `match_on` key names a top-level element slot; the key `type` matches the element's `type:` CLAIM, not a field.
- the list applies in ORDER, each entry seeing the effect of the ones before it: DIFFERENT fields are independent ensures; the SAME field takes N order-stable appends into one list, each deduped by its own `match_on`.
- OPAQUE: the engine validates nothing about any `field` / `record` / `match_on`'s meaning, and a record violating the field's declared type surfaces as an ordinary advisory diagnostic on the next build, never a rejection. Un-forgeability is the mediator's (being the sole write path), not the engine's.
- all the stamps share the write's single commit; a dedup no-op contributes no change (so the commit carries only the changes the non-no-op stamps and the primary write produced, or nothing if all were no-ops). Off-git they apply to the file without committing, the plain-write degrade.
- every stamp targets the verb's primary file: `write_file` / `edit_file` / `assign_block_id` / `rename_block_id` / `edit_record` / `append_record` stamp `path`, `rename` the destination `to`, `promote` the new `to`, `inline` the host `into`.
- rejections: any stamp on a TYPE-DEF target (a `*.type.yaml`, or a file under a `type/` dir — a stamp rides an instance's frontmatter, not a def); any stamp on `rename_type` (a cascade with no single target file). Targeting another file in the write set (the spec's `path`) is deferred.
- since `type` inside a record is a claim (rendered `type:`-first), the stamped file must be a frontmatter file or a markdown file (which GAINS a `---` frontmatter block when it has none); a pure-`.yaml` instance is stamped as its own whole-file record.

Ensure-mixins rider (`schema_version` 25): `write_file`, `edit_file`, and `rename` accept an optional `ensure_mixins`, a LIST of `::repo`-qualified type names the engine idempotently ensures on the written file's `type:` CLAIM, folded into the write's OWN commit ([[spec - ensure-mixin write directive - a governed write ensures a type-claim mixin idempotently, folded into the write's own commit]]). The type-claim analog of the stamp rider's field-append, so a `full`-mode provenance write leaves a file both stamped and typed, validating cleanly.
- shape: `ensure_mixins: ["provenance::au-provenance", ...]` plus `ensure_mixins_strict` (boolean, DEFAULT true). An absent or empty list is no mixin.
- TYPE-AWARE, not opaque: the engine resolves each mixin and applies it only when it can, per entry:
  - NO-OP when the claim's closure already includes the mixin (claimed directly, or a claimed type extends it).
  - APPLIED (the name appended to `type:`, or a `type: <mixin>` claim CREATED on a claimless note) when the append introduces no new ERROR-severity diagnostic on the file.
  - UN-APPLIABLE when the append would introduce a new error — an unresolvable or non-dependency repo, a non-claimable (sealed/abstract) target, a mixin-collision, or a newly-unmet required field. A new warning / hint / drift never blocks (open-world).
- `ensure_mixins_strict` decides an un-appliable mixin's fate: STRICT (the default) REJECTS the whole write, the reject naming the mixin and reason, nothing lands; LENIENT (`false`) SKIPS the mixin, the write lands with the file's advisory interim state.
- the response reports each entry under `ensure_mixins`, an array of `{ mixin, outcome: "applied" | "no_op" | "skipped", reason? }`, so a lenient skip is never silent. Omitted when the write carried no mixins. A strict reject is the `error` frame instead.
- ORDER: stamps fold first, then mixins, all into the write's one commit; a mixin validates against the already-stamped content.
- the mixin targets the verb's primary file: `write_file` / `edit_file` the `path`, `rename` the destination `to`. A mixin on a TYPE-DEF target (a `*.type.yaml`, or any file under a `type/` dir) rejects — a type-def's `type:` is a parent claim, not an instance identity. The same shared guard rejects a `stamps` rider on that target too.
- the remaining stamp-accepting verbs (`assign_block_id`, `rename_block_id`, `edit_record`, `append_record`, `promote`, `inline`) do NOT accept `ensure_mixins` yet, so an `ensure_mixins` field on them is an unknown-field reject; they gain it consumer-driven when a stamper emits a mixin on a structural refactor.

Attribution rider: `write_file`, `edit_file`, and `delete_file` accept an optional `attribution`, a LIST of `{ key, value }` trailers the engine folds into the mutation's commit beside `Mutation-Id`, VERBATIM and UNINTERPRETED, so a `session` / `span` stays a caller concept and the engine stays domain-pure ([[spec - git write path - commit-per-mutation as a local saga over the workspace's materialized repos]]). Read back by `commit_meta`'s `trailers`.
- shape: `attribution: [{ key, value }, ...]`. An absent or empty list writes no attribution.
- a key must be a well-formed trailer token (non-empty, no `:` or newline; a value carries no newline) and may NOT collide with a reserved engine key (`Mutation-Id` / `Mutation-Members` / `Moved` / `Reverts` / `Traced`, case-insensitive). A malformed or reserved-colliding key REJECTS the whole mutation before any write, an `error` frame, so nothing lands half-attributed. The reservation stops a caller forging an engine record through the attribution slot.
- ADVISORY, trace-tier: a trailer is unsigned free text, forgeable, and its `sha`-mapping rides the append-only invariant, so a consumer treats read-back attribution as a claim, not proof. Additive optional input, no `schema_version` bump.
- the other write verbs (the refactors, `assign_block_id`, the record edits) do not accept `attribution` yet; they gain it consumer-driven.

**write_file.**
- args: `path`, `content`, optional `expected_hash`, optional `stamps` (the Stamps rider above), optional `ensure_mixins` (the Ensure-mixins rider above), optional `attribution` (the Attribution rider above).
- full-content write, parent directories created.
- `expected_hash` is the read-before-write guard: the hash of the content the caller last read. A mismatch rejects without writing; `detail.current_hash` carries the current one.
- absent `expected_hash` means overwrite-regardless; creating a new file needs none.

**edit_file.**
- args: `path`, `old_string`, `new_string`, optional `replace_all` (default false), optional `stamps` (the Stamps rider above), optional `ensure_mixins` (the Ensure-mixins rider above), optional `attribution` (the Attribution rider above).
- exact string replacement; `old_string` must match exactly and be unique, `replace_all` lifts uniqueness.
- a non-unique match rejects naming the count (`detail.occurrences`); a miss rejects — the exact match is its own read-before-write precondition, no hash arg exists.
- identical `old_string`/`new_string` and unreadable files reject.

**delete_file.**
- args: `path`, optional `expected_hash`, optional `attribution` (the Attribution rider above).
- removes the file; the rebuild drops its node from the graph.
- `expected_hash` is the read-before-write guard: a mismatch rejects without deleting (`detail.current_hash`).
- deleting an absent file rejects — no silent success.
- the result's `hash` is null (the target is gone) and `reflected` is true (no content to lag).
- the result adds `last_live_commit`: the LAST-LIVE commit, the parent of the deletion commit (HEAD immediately before the delete), the last commit where the file still existed. Present only on-git; absent off-git (no deletion commit, nothing to pin). Distinct from `commit` / `commits`, which name the DELETION commit for attribution. A consumer's delete tombstone pins `[[<path>::@<last_live_commit>]]` so the file's last content reads back — pinning the deletion commit would give an absent target (`pinned-path-absent`). The clean-at-HEAD precondition guarantees the file was present at the parent, and an untracked (never-committed) file's delete rejects before any commit, so a surfaced `last_live_commit` always resolves the file. Additive, no `schema_version` bump. See [[spec - git write path - commit-per-mutation as a local saga over the workspace's materialized repos]].

**assign_block_id.**
- args: `path`, `at` (a byte offset inside the file, e.g. from a span the wire served).
- the engine resolves the enclosing addressable entity and assigns a `^:` id:
  - a YAML inline record gains `^: <id>` as its first key (block-style only; flow-style rejects).
  - a markdown block gains a trailing ` ^<id>` marker at the block's end.
- result adds `{ id, ref }` — the ref in `[[target^id]]` form, bare stem when unique, repo-relative path otherwise; it resolves through `resolve_block_id`.
- assigning to an entity that already carries an id is idempotent, for both surfaces: an inline record already carrying `^:`, or a markdown block whose end already bears a `^<id>` marker. The existing id comes back, nothing is written, no commit.
- ids are engine-generated (`b-` prefixed), collision-checked against the file.
- the offset is interpreted against the held parse; a disk file that drifted from the catalog rejects — wait for the rebuild and retry with a fresh offset.
- an offset enclosed by no record and no markdown block rejects.

**edit_record.**
- args: `path`, `field_path`, `patch`, optional `expected_hash`, optional `on_invalid` (`advise` default | `reject`).
- patches a nested typed record's fields in place, keyed by the `instances_of` `field_path` locator (an array of field names and list indices, e.g. `["phases", 0, "actions", 1]`; an empty array addresses the file-level instance).
- a byte-splice: only the touched field bytes change, every other byte, and every comment, stays identical. See [[spec - nested record edits - patch a record and append to a sequence by byte-splice, comments preserved]].
- `patch` is a shallow map of field name to value (JSON, rendered to YAML by the engine):
  - `type` re-types the record, replacing the claim (a single name renders bare, several render inline `[a, b]`).
  - an existing field is replaced in place; v1 replaces a SCALAR field value only (a list/mapping field replace rejects, it would re-render and drop comments).
  - an absent field is inserted at the record's field indent, after its last authored line.
  - a `^` block-id key rejects (that is `assign_block_id`'s), and an empty `patch` rejects.
- `expected_hash` is the read-before-write guard, same as `write_file`: a mismatch rejects (`detail.current_hash`).
- `on_invalid`: `advise` (default) lands the write and surfaces the record's diagnostics; `reject` refuses when the change RAISES the file's validation-error count, writing nothing, the reject naming the new codes. A pre-existing error never blocks.
- rejects loudly (nothing written): a flow-style record (`{ .. }`), a `field_path` resolving to no record, a sequence `field_path` (that is `append_record`), a scalar field replaced by a list/mapping, or a splice that would produce unparseable content. Operates on an instance file.

**append_record.**
- args: `path`, `field_path`, `value`, optional `expected_hash`, optional `on_invalid`.
- appends one element to the sequence the `field_path` addresses, a byte-splice after the last element so every existing element and its comments stay identical.
- an empty `[]` sequence is seeded, rewritten to a block list with the element as its first item. A BARE `key:` (a null value, no `[]`) is not a seed target and rejects — author `[]` for an emptyable list.
- `value` is the element to append (a record or a scalar), rendered to YAML by the engine.
- `expected_hash` and `on_invalid` behave as for `edit_record`.
- rejects loudly: a populated flow-style sequence (`[a, b]`), a `field_path` resolving to no sequence, or a record `field_path` (that is `edit_record`). Physical instance files only.

**rename.**
- args: `path`, `to`, optional `stamps` (the Stamps rider above; folded into the renamed file `to`), optional `ensure_mixins` (the Ensure-mixins rider, on `to`).
- moves the file from `path` to `to` and rewrites every inbound reference to point at the new name, all as one mutation.
- the move is same-repo (v1): a cross-repo `to` rejects. Referrers may be in any mounted repo; each is rewritten in its own repo, and the move plus every rewrite commit as one saga — one `Mutation-Id`, shared across every repo the mutation touches.
- the reference rewrite preserves the referrer's spelling mode (`[[old]]`, `[[notes/old]]`, `[[old.md]]` stay bare / path / extension), every fragment (`#anchor`, `^block`, `:field`), and the `::repo` qualifier; only the name changes.
- **a commit-pinned referrer is FROZEN, and this holds for every reference-rewriting verb** (`rename`, `promote`, `inline`, `rename_block_id`, `rename_type`). A `[[old::@sha]]` names its target as of `sha`, so re-pointing it would edit a historical record into a false claim; it survives byte-identical, fragments included (a `^block-id` on a pin names the id as of that commit too). Unpinned referrers in the same file and the same mutation are rewritten as usual. The consumer-visible consequence: a commit-pinned reference forms NO inbound backlink at all, it is an inert snapshot and resolution never attaches it to the live file bearing the name, so it never appears in an inbound-edge query (`references_in`) and a rewrite never touches it. Its pin stays visible on the source's `references_out` with the commit. See [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
- guards (the refusal surface): `path` must exist; `to` must not exist, nor collide case-insensitively with a sibling (a case-only rename rejects too); `to` must resolve to the same repo as `path`; a type-def file (`*.type.yaml`, or any file under a `type/` directory) rejects — a type-def's name derives from its filename, so moving it is `rename_type`'s job, not this rewrite; the path-escape guard applies to every touched path, and the clean-at-HEAD guard to every touched path in a git repo (the move's two paths and every referrer); every rewritten referrer is re-checked against its held content hash, so one that drifted on disk (committed, not yet rebuilt) rejects rather than being rewritten at a stale span; a non-git member is the documented transition fallback, written without the clean check and not rolled back.
- a file that references itself has its own reference rewritten too, so the moved file points at its new name.
- the result's `hash` is the moved content's hash at `to`; `reflected` follows the rebuild like any write.

**promote.**
- args: `path` (the host file), `to` (the new file's path), and exactly one record locator: `at` (a byte offset inside the record) or `block_id` (the record's `^:` id).
  - `at` addresses a record with no id; `block_id` is the stable handle for one that has it (a referenced record always does). Both supplied, or neither, rejects.
- the engine resolves the inline record, lifts it into its own file at `to`, leaves a quoted `"[[newFile]]"` reference where the record sat, and rewrites every inbound `[[host^id]]` referrer to `[[newFile]]` — the extraction, the host edit, and every referrer rewrite commit as one saga, one `Mutation-Id` across every repo touched.
- the referrer rewrite is a form change: the block-id and anchor drop, the target swaps to the new file, the `:field` attribution and `::repo` qualifier carry. Mounted cross-repo referrers are rewritten; an unmounted peer's referrers are not rewritten, recovering them is deferred to a future cross-repo sync.
- a record is a subtree: its own `^:` id plus every nested `^:` id within it all travel to the new file. The promoted record's own id collapses to `[[newFile]]`; a referrer to a nested id keeps its id under the new file (`[[host^nested]]` becomes `[[newFile^nested]]`). Intra-record references to a travelling id are rebased onto the new file too, so nothing the record carried dangles.
- the new file is the record's YAML, de-indented to a top-level document with its `^:` id dropped; a `.md` (or `.markdown`) `to` is fenced (`---` frontmatter), a `.yaml`/`.yml` `to` is the bare YAML. The `to` extension is the format selector and must be one of these; any other rejects.
- a referrer's MODE decides its rewrite: a `^^id` block-referent (the record IS its value) follows the record to the new file; a slot-less prose `^id` follows navigationally; a bare `^id` that FILLS a slot (a reference value or a `:field` contribution) is navigational with the HOST as its value, so promote rejects rather than silently repointing it to the extracted record (see the guards).
- a referrerless record (no `^:` id, or a `^:` id with no inbound references) is pure extraction, nothing else rewritten — cross-repo-safe by construction.
- a reference inside the record to its own block-id is redirected to the new file (the record now *is* that file), so the extracted content carries no dangling self-reference.
- guards (the refusal surface): `path` must be a typed instance in the knowledge base; `at`/`block_id` must resolve to an inline record; the record's slot must admit a reference (an `&` inline-or-reference slot) — a bare inline-only slot (`T`, `any`, a non-`&` compound) cannot hold the `[[newFile]]` reference, so it rejects; `to` must resolve to the same repo as `path`, must be a `.md`/`.markdown`/`.yaml`/`.yml` file (not a type-def), and must not already exist; the locator is interpreted against the held parse, so a host that drifted from the catalog rejects (wait for the rebuild and retry); a bare `^id` referrer that fills a slot rejects — it is navigational with the host as its value, so following the record would silently change that value, make it `^^id` (to follow the record) or drop the anchor and retry; every rewritten referrer is re-checked against its held content hash, so one that drifted on disk (committed, not yet rebuilt) rejects the whole refactor rather than being rewritten at a stale span; the clean-at-HEAD guard applies to every touched path in a git repo.
- the result's `hash` is the new file's content hash; `reflected` follows the rebuild like any write.

**inline.**
- args: `path` (the file to fold in and delete), `into` (the host referrer that hosts the record), `at` (optional, a byte offset in `into` landing on the `[[path]]` reference to host — required when `into` references `path` more than once).
- the dual of promote. The chosen `[[path]]` reference in `into` becomes an inline `^:id` record holding `path`'s frontmatter; every other whole-file `[[path]]` in `into` becomes the local `[[^id]]`; every cross-file whole-file `[[path]]` becomes `[[into^id]]` (repo qualifier and fragments preserved); `path` is deleted. All as one saga, one `Mutation-Id`.
- `path` is itself a subtree: any nested `^:` id it declares travels under the new record, keeping its id. A referrer to a nested block, `[[path^child]]`, becomes `[[into^child]]` (not the outer record id), and an in-`into` one becomes the local `[[^child]]`. `path`'s references to its own blocks travel with the content and are rebased to the local form so they resolve in `into` after the fold.
- block-ids are file-local-unique. A nested `^:` id in `path`'s subtree that collides with an id `into` already declares (record or body marker) is renamed to a fresh id during the fold, and every reference to it follows; `into`'s own id is untouched. The engine-assigned record id and any collision renames are drawn clear of both files' id namespaces.
- the record is `path`'s frontmatter re-indented into the slot with the engine-assigned `^:` id; a sequence element keeps its `- ` marker, a field's record moves to the following lines.
- guards (the refusal surface): `path` and `into` must be distinct typed instances in the same repo; `path` must carry no non-empty body (a record has no body — strip it first, or keep the file); `into` must reference `path` (else it is not a referrer); a multi-reference `into` needs `at`, and `at` must land on a reference to `path`; the chosen host slot must admit the inlined record (an `&` inline-or-reference slot) — a reference-only `*` slot holds the `[[path]]` reference but cannot hold the record, so it rejects; every other referrer must be able to hold a block-id — a `file*` whole-file slot and a `[[path#head]]` anchor cannot, so they reject, the offenders named in the response `detail.offenders`; the held-parse hashes guard read-before-write on both files, and every rewritten cross-file referrer is re-checked against its held hash too (a committed-but-unrebuilt drift rejects rather than mis-rewrites); the clean-at-HEAD guard applies to every touched path in a git repo.
- the result's `hash` is the host's new content hash; `reflected` follows the rebuild like any write.

**rename_block_id.**
- args: `path` (the host file), `block_id` (the record's current `^:` id), `to_block_id` (the new id).
- renames an inline `^:id` record's block-id in its host: the `^:` declaration becomes `to_block_id`, and every referrer filtered to that id follows — a cross-file `[[host^id]]` becomes `[[host^to_block_id]]`, a host-local `[[^id]]` becomes `[[^to_block_id]]` (target, anchor, `:field`, and `::repo` preserved). The declaration edit and every referrer rewrite commit as one saga, one `Mutation-Id`. The block-id sibling of `rename` (which moves a file); both ride the same rewrite core.
- scope: inline `^:` records only. A body-marker `^id` (on a paragraph, heading, or fenced block) is not renamed — the locator rejects, since no inline record carries the id.
- guards (the refusal surface): `to_block_id` must match the block-id grammar (`[A-Za-z0-9_-]+`) and differ from `block_id`; `path` must be a typed instance in the knowledge base carrying an inline record with `block_id`; `to_block_id` must be free in `path`'s file-local id namespace (both inline records and body markers), else it rejects to avoid a duplicate; the held-parse hash guards read-before-write on the host, and every rewritten referrer is re-checked against its held hash too (a committed-but-unrebuilt drift rejects rather than mis-rewrites); the clean-at-HEAD guard applies to every touched path in a git repo.
- the result's `hash` is the host's new content hash; `reflected` follows the rebuild like any write.

**rename_type.**
- args: `old_name`, `new_name`. `old_name` is bare (`foo`) or `::repo`-qualified (`foo::repo`). A `::repo` selects WHICH owner's identity to rename when two mounted repos own the name; a bare name picks the first owner across the mounted set. `new_name` is always the owner's own new name, so it stays bare.
- renames a type-def: the name derives from the filename, so the def file moves (`<old_name>` to `<new_name>`, suffix and directory kept) and every reference to the type follows, all as one saga, one `Mutation-Id`. The type-vocabulary sibling of `rename`, which refuses a type-def file because its rewrite only touches wikilinks.
- two reference surfaces are rewritten together, merged per file:
  - the type-name references in the owning repo — `type:` claims (parent and identity), `sealed:` branches, slot shapes (`name*`, `type<name>*`, `<name | other>`, …), qualified keys (`field{name}`), body `use: name`, meta `- type: name`, and nested inline-record `type:` claims. None is a wikilink. Repo-local: a same-named type in another repo is its own, untouched.
  - the wikilinks to the def file across the mounted set — a `[[name]]` def-ref value or navigational link, and `file*`/path references, each re-pointed in its spelling mode (the type-name `[[name]]` follows to `[[new_name]]`, not normalised to the def-file stem). An unmounted peer's referrers are not rewritten, recovering them is deferred to a future cross-repo sync, like `rename`.
- the def-file move writes a `Moved: <old> -> <new>` trailer on the committing working tree's commit, its paths relative to that tree, so a commit-pinned `type<T>*@` reference whose def was renamed traces forward. For a member nested in a larger tree the trailer's paths are therefore rooted above the member.
- guards (the refusal surface): `old_name` must name a type-def in the workspace; `new_name` must match the type-name grammar and must not already name a type-def in that repo (a duplicate rejects); every rewritten referrer is re-checked against its held content hash (a committed-but-unrebuilt drift rejects rather than mis-rewrites); the clean-at-HEAD guard applies to every touched path in a git repo.
- the result's `hash` is the moved def file's content hash at its new path; `reflected` follows the rebuild like any write.

**set_ignores.**
- args: `root` (a member root, exactly — the `ignores` read surfaces it), `patterns` (the full replacement `.auignore` list; an empty list removes the file, reverting to the default excludes).
- replaces a member's `.auignore` scope rules through the governed channel, then re-scopes. A CONFIG-sort mutation, a distinct sort from the graph mutations above: it edits which content enters the graph, not graph content.
- the sequence is the standard shape: validate the patterns up front (build the same `WalkFilter` the build uses — a malformed pattern REJECTS with the reason, nothing written, the governed path's win over a raw file write that would degrade it to the `auignore-load-error` advisory); write (or, empty, remove) `<root>/.arsumbris/.auignore`; commit per mutation with a `Mutation-Id` trailer, like any governed write; re-scope (the `.arsumbris/` change forces a full re-walk); respond with fresh state.
- the ONE sanctioned write under `.arsumbris/`, and only to `<root>/.arsumbris/.auignore`. Every other `.arsumbris/` path stays guarded for every verb, this one included — `set_ignores` does not lift the guard, it constructs the single allowed path from a validated member root. A raw `write_file` to `.arsumbris/.auignore` still rejects.
- the integrity contract is deliberately relaxed: a scope mutation MAY orphan references (newly excluding a referenced file is a valid scope choice), which surface as advisory `reference-target-missing` / `navigational-target-not-found` diagnostics, never a rejection. The graph mutations' strict no-dangling contract is unchanged; this is the config sort's distinct contract, see [[spec - scope management surface - an ignores read and a set_ignores config mutation]].
- `.auignore` is out-of-band config, never a catalog entry, so the result's `hash` is null and `reflected` is the delete-style "nothing to lag"; the re-scope runs before the response, so the returned `version` already reflects the new scope. Additive, no `schema_version` bump.
- rejects a `root` that is not a declared member root, and a malformed pattern (nothing written in either case).

**set_config.**
- args: `{ scope, consumer, file, type, content, root?, expected_hash? }` — the write dual of the `config` read, one CONSUMER config file under `<scope>/<consumer>/config/<file>`.
  - `scope` / `consumer` / `file` / `type` / `root`: as the `config` read. `type` is the DECLARED floor.
  - EXACTLY ONE of `content` / `edit`:
    - `content`: the whole file to write.
    - `edit`: `{ field_path, patch }` — one keyed-record splice over the EXISTING file, reusing `edit_record`'s byte-splice core, so comments, key order, and sibling records survive. `field_path` locates the record (empty = the file's top-level record); `patch` is a shallow map of field name to scalar value (`type` re-types a record with an explicit claim, `^` is rejected). No `on_invalid` gate: a config write is advisory (never refused on a field-shape verdict), and the splice's own structural check still rejects a change that would break the YAML. Editing an ABSENT file rejects (create it with `content` first).
  - `expected_hash`: optional compare-and-set against the current file's content hash; a mismatch (or an absent file) REJECTS, nothing written.
- a CONFIG-sort mutation, a second sanctioned bypass of the `.arsumbris/` write-guard (like `set_ignores`): path-safety and the reserved `au-engine` owner segment gate the two caller-controlled segments, the path is constructed directly. A raw `write_file` under `.arsumbris/` still rejects.
- the write INJECTS `type: <declared>` when the content does not already declare its own top-level `type:` (a written one WINS and may subtype), so a governed write self-describes.
- REPO scope commits per mutation through the saga (a `Mutation-Id` trailer), the result carries `commit` / `commits`. The file rides the `.arsumbris` floor, so it is out-of-band: no catalog entry, no re-scope, no rebuild — `reflected` is the delete-style "nothing to lag", and the result's `hash` is the WRITTEN file's content hash (surfaced explicitly for the next compare-and-set), not a catalog hash.
- relaxed integrity: a field-shape verdict is advisory, the write is never refused on it (an unresolved `type` stores with `config-type-unresolved`, an absent `type:` key with `config-type-unwritten`).
- MACHINE scope writes a device-global file under `~/.arsumbris/<consumer>/config/<file>` (the engine's `ConfigSource`, injectable for tests) with NO git commit and NO rebuild, so `commit` is null and `commits` is `{}`. A cross-daemon exclusive file lock (the same `flock`-on-a-sibling pattern `register` uses) serializes concurrent writers of the one device-global file, and the write is atomic (temp + rename), so a reader never observes a torn file. A NEW mutation verb, additive, no `schema_version` bump.
- rejects a path-unsafe / reserved `consumer` / `file`, an unknown repo-scope `root`, and an `expected_hash` mismatch (nothing written).

**register.**
- args: `name` (the repo to register), `path` (its local absolute path on this machine), `remote` (optional git remote).
- writes or updates one `{ name, remote, path }` entry in the per-user registry `~/.arsumbris/au-engine/config/repos.yaml`, the locator for a scattered repo, then rebuilds so a newly-locatable member mounts. The consumer-driven bootstrap for a `peer-unmounted` dep: a folder picker supplies `path`, `register` records it.
- a CONFIG-sort mutation over a DEVICE-GLOBAL file. Unlike `set_ignores`, the file sits OUTSIDE every repo, so there is NO git commit. The registry path is under the engine's device root (`$HOME/.arsumbris/au-engine/config/repos.yaml`), injectable for tests.
- refuses (an `error` frame, nothing written) when: `name` is not a valid repo name; `path` or `remote` carries a newline (a line-oriented-registry breakout); the name's identity disagrees, `dependency-identity-conflict` (an existing entry with a different `remote`, or a `path` whose `repo.yaml` declares a name other than `name`); or the `path` has no readable / parseable `repo.yaml` (register-before-clone is not supported, the path must be an existing checkout that declares `name`). The identity check is the same one resolution runs, so a mis-registration is caught up front, never silently resolved later.
- a bare `{ name, path }` re-register (no `remote`) PRESERVES the entry's recorded remote rather than clearing it.
- a successful register answers a `registered` frame: `name`, `path` (echoed, absolute), and `version` (the post-rebuild held version). Additive, no `schema_version` bump.
- rejects when the device root is unresolvable (`$HOME` unset).

Mutating while the ref is Deriving answers not-ready, like a read.


## The resolve verb

Resolves the served workspace's declared dependency closure, see [[spec - package manager - git-ref dependencies resolved through a registry repo into a device-local cache]].
Engine-internal, distinct from the mutation catalog: it writes a per-repo package lock under each editable repo's `.arsumbris/` and rebuilds, it is not a `read`. Variant-less, its arguments object is empty.

```json
{ "resolve": {} }
```

- fetches the declared dependency closure into the device-global package cache, by explicit `remote`/`ref` or by name through the registry, and mounts each read-only from its immutable snapshot. The closure is transitive: each fetched dependency's repo-root `deps:` are resolved too, cycle-safe, so the user declares top-level intent and cannot forget a transitive dependency.
- writes a PER-REPO lock, `<repo>/.arsumbris/repo.lock`, for each EDITABLE member (a local working tree), pinning that repo's OWN full transitive fetched closure (`name → sha`), then commits each into the git tree that holds it. A shared `Mutation-Id` trailer correlates the whole solve across the per-repo commits. The dependency lock is per-repo, since deps are declared per-repo in `repo.yaml`.
- the pipeline is synchronous like a mutation: fetch → write each repo's lock → commit each → rebuild → respond.
- the resolver is per-repo independent: a dependency that cannot be delivered is reported in `failed`, the rest still lock and mount. Each committed lock records only what resolved, each snapshot atomic, so it never references a half-fetched package.
- a package required at two versions across the closure (the same `(remote, path)`, two shas) is a hard conflict: the engine never picks a winner, so the conflicted package is excluded from the locks and the mounts and reported in `conflicts`. It is never mounted at one arbitrary version.

A successful resolve answers a `resolved` frame:
- `resolved`: the resolved members, each `{ name, sha, remote, path }`. `path` is the monorepo subpath, null for a repo-root package.
- `failed`: the members that could not be delivered, each `{ name, reason, code }` where `code` is `dependency-resolution-failed`. Empty in the common case. A non-empty list means resolve was partial.
- `conflicts`: one message per package required at conflicting versions. Empty in the common case. A non-empty list means a conflicted package was left unresolved and must be aligned.
- `commits`: a map `{ [repo]: sha }`, one entry per editable repo whose own `repo.lock` committed this resolve. Empty when nothing resolved, no repo is a git tree, or every lock was unchanged (the idempotent no-op). One `Mutation-Id` correlates the whole set. Same shape as the mutation frame's `commits`. **`schema_version` 10**: this replaces the former single `commit` / `commit_error` scalars, since the dependency lock moved from a per-workspace file to a per-repo one.
- `commit_errors`: a map `{ [repo]: message }`, one entry per repo whose lock commit failed with a git error. A present entry means that repo's lock is on disk but uncommitted (soft-inconsistent). Empty in the common case.
- `version`: the post-resolve knowledge base version.

Tree/ad-hoc mode (a directory entry, no manifest) resolves too: every co-present repo is a primary, and each writes its own `.arsumbris/repo.lock`. A directory with no co-present repo resolves nothing (an empty `resolved` / `commits`), not an error.
Resolving while the ref is Deriving answers not-ready, like a read.


## The control verb

**shutdown.**
- not a read.
- the signal `au daemon stop` sends.
- the daemon acks, then exits and removes the socket.


## Consumer mapping

The consumer ports each bind to one or more reads.

Notes are markdown files with no `type:` claim, or no frontmatter at all.
They are first-class: the engine holds their frontmatter and body.
So the reference, frontmatter, and content reads serve them like typed instances.
Notes are not validated and have no typed value layer, so `instance` returns null for a note.

- **DiagnosticsPort** `validate` ← `diagnostics`.
  - `range { file, start, end }` ← `span { file, range { start, end } }`.
  - `suggestedFix` ← `fix`.
  - `related[].range` ← `related[]` span.
  - severity is lowercase, the engine emits `error` / `warning` / `hint`.
- **TypeIndexPort** `listTypes` ← `types`, `listInstancesOf` ← `instances_of`.
  - `definedAt` ← `source.file`, `typeClaim` ← `claim`.
- **BodyTemplatePort** `getBodyTemplate` ← `type`.body, `getEffectiveBody` ← `type`.effective_body.
- **ProvenancePort** `getEffectiveValues` / `getSectionPresence` / `getBodyEvents` ← `instance`.
  - typed instances only, a note has no typed value layer.
- **LinkGraphPort** `getOutgoing` ← `references_out`, `getBacklinks` ← `references_in`, `resolveTarget` ← `resolve_target`, `resolveBlockId` ← `resolve_block_id`.
  - all serve notes and typed instances alike.
- **RepoFilesPort** `listChildren` ← `dir_entries`, `readFrontmatter` ← `frontmatter`.
  - `readFrontmatter` serves notes and typed instances.
- **TopLevelGraphsPort** `list` ← `top_level_dirs`.

The standing guard against drift is `tests/port_contracts.rs`, one test per port.
