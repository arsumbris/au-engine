//! IPC over a Unix domain socket: framed JSON, reads and live subscriptions.
//!
//! One endpoint per entry point, at the path [`socket_path`] derives, so the
//! daemon and any consumer reach it from the entry path alone. Each frame is a
//! length-prefixed JSON object. A request names either a `read` or a
//! `subscribe`; every outbound frame is tagged by a `type`, so a consumer
//! demultiplexes responses and events on the one connection. The full contract
//! is `WIRE.md`.
//!
//! The read catalog is closed, a per-capability set from [[design - engine
//! shape]]: diagnostics (whole-knowledge-base, or scoped by path / path-prefix /
//! severity / code); the type graph (all type-defs, one by
//! name); instances of a type by closure membership; the knowledge-base-wide
//! per-instance introspection and the implicit-identity candidate scan; one
//! instance's resolved view with its full value layer; references (a file's
//! outgoing links,
//! backlinks, resolve a target, resolve a block-id); the knowledge base (a directory's
//! children, a file's frontmatter, a file's content, the top-level graphs);
//! the engine's own type-system reference (the embedded atomic specs);
//! and a ready probe surfacing the Deriving-to-Ready transition. A read's
//! `response` frame carries a schema version, the readiness, the observed
//! version, and the result. Reads serve the held analysis; content is the one
//! read that returns source-form text from the working tree.
//!
//! The subscription catalog is likewise closed: lifecycle, types, files,
//! changes, the parametric diagnostics channel, the drawable link_graph and
//! type_graph, and recent_commits, scoped by the same filters as their reads. A
//! `subscribe` is answered by
//! an `ack`, then an `initial_value` when the channel has one, then
//! `change_event` frames. Most channels wake on the engine's version signal;
//! recent_commits is the exception, a git-state projection woken by an
//! on-demand reflog watcher (off the version signal), see [`run_recent_commits`].
//! Subscriptions live with the connection and die when it closes. The
//! connection runs a writer task draining a frame channel, so a response and an
//! event never interleave a partial frame, and one task per subscription. Each
//! data channel diffs its own projection across rebuilds and carries the delta
//! in its change events' scope hint: changed files, changed paths, changed
//! type names, files whose diagnostics changed.
//!
//! Alongside both is one control verb, `shutdown`, the lifecycle signal `au
//! daemon stop` sends; a foreground server parks on
//! [`ServeHandle::wait_for_shutdown`] until it arrives.
//!
//! The serve layer is async on a private tokio runtime; the analysis core stays
//! sync, its held-state lock taken briefly inside a handler and never across an
//! await. Windows named-pipe transport is deferred; this is Unix-only. The wire
//! shape (length-prefixed JSON) is transport-agnostic and would carry over.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read as IoRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use au_diagnostics::{Diagnostic, Severity};
use au_grammar::{parse_shape_spanned, Shape, ShapeSpanRole};
use au_parser::{scan_body, BodyEvent};
use au_references::{extract_field_marker, parse_wikilink, parse_wikilink_inner};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::Instrument;

use crate::engine::{EngineHandle, Read, RefState};
use crate::gitwriter::{CommitMessage, CommitSha, GitWriter, MutationId, ShellGit};
use crate::mutate::MutationReject;
use crate::parse::{served_file_kind, FileParse};
use crate::wire;
use crate::{KnowledgeBase, RefSurface};

/// The wire schema version stamped on every frame. v29: an `inline_record` value
/// (in `effective_values`) replaces its raw-mapping `value` with `fields`, each
/// nested field resolved through the value model exactly like a top-level field
/// and recursively to any depth, so a tuple / brand / reference inside a nested
/// record reads resolved, not as a raw string. BREAKING (a type-change of an
/// existing field). v24: the write verbs' `stamp`
/// rider (a single stamp) is replaced by `stamps`, a LIST of stamps applied in
/// order into the write's one commit, so many stampers fold into one mutation. A
/// length-1 list is exactly the former single stamp. v22: `references_out` gains a
/// new `kind` value `commit-referent`, for a commit-only reference
/// (`[[::@sha]]` / `[[::repo@sha]]`) that names a commit rather than a file. Its
/// `resolved` is null and `commit` is set, but it is NOT dangling. v18: a type view
/// (`TypeIntrospection`) gains three fields for the abstract / required-meta
/// batch: `abstract` (bool, the `abstract: true` marker), `required_meta`
/// (String[], the type's own `required:` obligations, authored forms), and
/// `unmet_required_meta` (String[], the required-meta names a non-abstract type
/// does not satisfy, empty when satisfied / exempt / no resolution graph). v17
/// (prior). v16: the `members` and
/// `resolve_member` reads drop the `primary` boolean and gain a `local` boolean
/// (a live working tree vs a read-only cache snapshot, the LOCATION axis) and a
/// `role` string (`entry` / `edit` / `discover` / `dep`); `editable` is now the
/// role axis (an editable authoring surface), no longer location-derived. A
/// consumer hides consumed members with `!editable` and scopes physical writes
/// with `editable && local`. v15: the hardwired
/// `au.engine.workspace` def is reshaped from a single `primary: String[+]`
/// selection to two optional lists, `edit?: String[]` and `discover?: String[]`,
/// and a new `au.engine.workspace-lock` def (`packages?`, parallel to
/// `au.engine.repo-lock`) pins the workspace's discover closure; a consumer
/// reads these on `type_system_reference` / `instances_of`. v14: (prior). v13:
/// `instances_of` returns
/// ALL instances of a type tagged by `origin` (file / nested inline record /
/// type-def meta), not only file-level; each match gains `origin`, a byte
/// `span`, and an origin-specific `locator`; the request accepts an `origins`
/// include-set filter (absent = all, a change from the old file-only default).
/// v12: a new `device_config`
/// read exposes the per-user device-global engine-schema files (`repos.yaml`,
/// `workspaces.yaml`), each file's path, content, and field-shape diagnostics
/// against its hardwired `au.engine.*` def. v11: the workspace-wide type
/// reads (`types`, `type_tree`, `type_counts`, `subtypes`, `list_imports`, and
/// the repo-scoped `types` / `type_counts`) accept an optional `scope: all | own`
/// argument (default `all`, unchanged); `own` hides every dependency and the
/// `au.engine.*` builtin, surfacing only the user's own primary, editable repos.
/// v10: the `resolve` frame
/// replaces its single `commit` / `commit_error` scalars with per-repo
/// `commits: { [repo]: sha }` / `commit_errors: { [repo]: message }` maps, one
/// entry per editable repo whose own `.arsumbris/repo.lock` committed, since the
/// dependency lock moved from a per-workspace file to a per-repo one. v9: the
/// `members` read
/// replaces each member's `provenance` string with two booleans, a derived
/// `editable` (a local working tree vs a read-only cache snapshot) and a
/// `primary` (a named primary vs a computed transitive dependency). v4: a
/// request may carry an `id`, echoed on its `response` / `ack` / `error` reply
/// so clients settle by id; `error` frames also carry a `for` discriminator. v3:
/// `resolve_target` returns `{ path, kind }` instead of the bare path string.
///
/// Public so a consumer (e.g. the `daemon status` surface) can compare a running
/// daemon's reported `schema_version` against the version this binary compiles,
/// and flag a stale process speaking an old wire.
pub const SCHEMA_VERSION: u32 = 29;

/// The largest frame payload accepted off the wire. A client-supplied length
/// prefix above this is a protocol violation, not a real request: the reader
/// rejects it before allocating, so a stray or hostile length cannot drive an
/// unbounded allocation. Generous for any genuine read, subscribe, or mutate.
const MAX_FRAME: usize = 16 << 20;

/// The serialized outbound frames a connection's producers hand to its writer
/// task. The writer drains this, framing one at a time, so responses and events
/// never interleave a partial frame on the socket.
type FrameTx = mpsc::Sender<Vec<u8>>;

/// The per-entry socket path, `~/.arsumbris/au-engine/run/<hash>.sock`.
///
/// One endpoint per entry point, derivable by both the daemon and a consumer
/// from the entry path alone, no configuration. `entry` is the absolute path the
/// engine was pointed at, a folder-repo directory, so each entry point gets its
/// own socket and many workspaces in one folder never collide.
///
/// The socket lives OUTSIDE the knowledge base, so a deep knowledge base path can no longer overrun
/// the Unix-socket `sun_path` limit (the SUN_LEN failure). The `<hash>` is the
/// FNV-1a of the entry path's bytes, the engine's own [`crate::ContentHash`], so
/// the file name is short and fixed-width and a consumer in any language derives
/// the same path by hashing the same bytes.
///
/// The caller passes a canonicalized absolute path (the CLI's `canonical_root`,
/// a consumer's realpath), so the daemon and a client hash an identical string.
pub fn socket_path(entry: &Path) -> PathBuf {
    sockets_dir().join(socket_file_name(entry))
}

/// The socket file name for an entry, `<hash>.sock`.
///
/// A pure deterministic function of the entry path's bytes, no env and no IO:
/// the FNV-1a hash (the engine's [`crate::ContentHash`]) rendered as fixed-width
/// hex. Exposed so a consumer, or a test with an injected home, derives the same
/// name the daemon binds under. Distinct entry paths hash to distinct names, so
/// each workspace file in a shared folder gets its own socket.
pub fn socket_file_name(entry: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let hash = crate::ContentHash::of(entry.as_os_str().as_bytes());
    format!("{:016x}.sock", hash.0)
}

/// The directory holding per-entry daemon sockets, `~/.arsumbris/au-engine/run`.
///
/// Read from RAW `$HOME`, not the canonicalized [`crate::device_root`], because
/// this path is a wire contract mirrored byte-for-byte by the SDK, which reads
/// `process.env.HOME` unresolved: both sides must agree, so both read `$HOME` as
/// given. Falls back to the system temp dir when `$HOME` is unset (a headless run,
/// unreachable on the daemon-start path, which requires `$HOME`), so a socket path
/// always exists for `stop` / `status`.
fn sockets_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".arsumbris")
        .join("au-engine")
        .join("run")
}

#[cfg(test)]
mod wire_doc_tests {
    use super::*;

    /// `WIRE.md` is hand-maintained with no generator, so its version summary can
    /// lag a [`SCHEMA_VERSION`] bump silently. This asserts the doc's `currently
    /// \`N\`` line matches the const, turning that drift into a caught build error.
    #[test]
    fn wire_md_summary_matches_schema_version() {
        let wire = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/WIRE.md"));
        // The single "current version" summary line, e.g. "currently `29`.".
        let marker = "the wire version, an integer, currently `";
        let start = wire
            .find(marker)
            .expect("WIRE.md must carry the wire-version summary line");
        let rest = &wire[start + marker.len()..];
        let end = rest
            .find('`')
            .expect("the version number must be closed by a backtick");
        let documented: u32 = rest[..end]
            .parse()
            .expect("the documented wire version must be an integer");
        assert_eq!(
            documented, SCHEMA_VERSION,
            "WIRE.md version summary (`{documented}`) is stale, bump it to \
             SCHEMA_VERSION (`{SCHEMA_VERSION}`)"
        );
    }
}

#[cfg(test)]
mod request_deser_tests {
    use super::*;

    /// The nested internal tags compose: `read` selects the preview verb, then
    /// `op` selects the mutation shape, with its args flat alongside.
    #[test]
    fn preview_mutation_request_deserializes_each_op() {
        let write: Request = serde_json::from_value(serde_json::json!({
            "read": "preview_mutation",
            "op": "write_file",
            "path": "a.md",
            "content": "x",
        }))
        .unwrap();
        match write {
            Request::PreviewMutation(PreviewMutationArgs::WriteFile {
                path,
                content,
                stamps,
            }) => {
                assert_eq!(path, "a.md");
                assert_eq!(content, "x");
                assert!(stamps.is_empty());
            }
            other => panic!("expected a write op, got {other:?}"),
        }

        let edit: Request = serde_json::from_value(serde_json::json!({
            "read": "preview_mutation",
            "op": "edit_file",
            "path": "a.md",
            "old_string": "x",
            "new_string": "y",
            "replace_all": true,
        }))
        .unwrap();
        assert!(matches!(
            edit,
            Request::PreviewMutation(PreviewMutationArgs::EditFile {
                replace_all: true,
                ..
            })
        ));

        let delete: Request = serde_json::from_value(serde_json::json!({
            "read": "preview_mutation",
            "op": "delete_file",
            "path": "a.md",
        }))
        .unwrap();
        assert!(matches!(
            delete,
            Request::PreviewMutation(PreviewMutationArgs::DeleteFile { .. })
        ));
    }
}

#[cfg(test)]
mod socket_path_tests {
    use super::*;

    /// The file name is a fixed-width `<16-hex>.sock`, no matter how deep the
    /// entry path — the SUN_LEN fix: a deep knowledge base path no longer feeds the
    /// socket path, a constant-length hash does.
    #[test]
    fn file_name_is_short_and_fixed_width_regardless_of_entry_depth() {
        let shallow = socket_file_name(Path::new("/a"));
        let deep = socket_file_name(Path::new(
            "/Users/someone/very/deeply/nested/repo/tree/that/would/overrun/sun_path/workspace",
        ));
        assert_eq!(shallow.len(), 21, "16 hex + '.sock'"); // 16 + 5
        assert_eq!(deep.len(), 21, "the deep path hashes to the same width");
        assert!(shallow.ends_with(".sock"));
        assert!(deep.ends_with(".sock"));
    }

    /// Deterministic: the same entry always derives the same socket.
    #[test]
    fn file_name_is_deterministic() {
        let a = socket_file_name(Path::new("/repo/kb"));
        let b = socket_file_name(Path::new("/repo/kb"));
        assert_eq!(a, b);
    }

    /// Distinct entries derive distinct sockets, so a directory and a manifest
    /// file under it (and two manifest files in one folder) never collide.
    #[test]
    fn distinct_entries_derive_distinct_sockets() {
        let dir = socket_file_name(Path::new("/repo/kb"));
        let manifest_a = socket_file_name(Path::new("/repo/kb/a/.arsumbris/workspace.yaml"));
        let manifest_b = socket_file_name(Path::new("/repo/kb/b/.arsumbris/workspace.yaml"));
        assert_ne!(dir, manifest_a);
        assert_ne!(manifest_a, manifest_b);
    }

    /// The socket lives outside the knowledge base, under the device-global
    /// `.arsumbris/au-engine/run/`, so it never sits in a watched member tree.
    #[test]
    fn socket_is_under_arsumbris_run() {
        let p = socket_path(Path::new("/repo/kb"));
        assert!(
            p.ends_with(
                Path::new("au-engine")
                    .join("run")
                    .join(socket_file_name(Path::new("/repo/kb")))
            ),
            "under .arsumbris/au-engine/run: {}",
            p.display()
        );
        assert!(p.to_str().unwrap().contains(".arsumbris"));
    }
}

/// A request over the socket, tagged by `read`.
///
/// Most variants are reads of the held analysis. `Shutdown` is the one control
/// verb: it is not a read of knowledge base state but the daemon-lifecycle signal `au
/// daemon stop` sends, asking the process to exit. It rides the same socket so
/// stopping needs no PID file.
#[derive(Debug, Deserialize)]
#[serde(tag = "read", rename_all = "snake_case")]
enum Request {
    /// Knowledge-base diagnostics, optionally scoped by [`DiagnosticsFilterArgs`],
    /// paged by `limit` / `offset`.
    Diagnostics(DiagnosticsFilterArgs),
    /// Counts over the filtered diagnostics set: total, by severity, by code.
    /// The shape of the problem without materializing every entry.
    DiagnosticCounts(DiagnosticsFilterArgs),
    /// Type-defs in the wire shape, each annotated with its repo. Absent `repo`
    /// is the workspace-wide read: every type-def across all members, deduped by
    /// identity to owner repos. `repo` scopes to one member's whole graph (its
    /// own defs). An unknown repo → null. Paged by `limit` / `offset`; projected
    /// to a lightweight summary by `summary`. See [`TypesArgs`].
    Types(TypesArgs),
    /// Counts over the `types` set for the same `repo` scope: a total plus a
    /// by-repo histogram. The shape of the vocabulary without materializing
    /// every def, the type dual of `diagnostic_counts`. An unknown repo → null.
    TypeCounts(TypeCountsArgs),
    /// The cross-repo type tree: the workspace's owner-deduped type-defs as a
    /// parent/child adjacency forest, owner-annotated per node. `scope: own`
    /// keeps only the user's own repos' types (see [`Scope`]).
    TypeTree {
        #[serde(default)]
        scope: Scope,
    },
    /// One type-def by `name`, annotated with its `repo` and identity `hash`;
    /// null when absent. Named by the authored form — bare (`foo`, the owner
    /// across the workspace) or `::repo`-qualified (`foo::repo`, the identity
    /// that repo holds) — with no separate `repo` key.
    ///
    /// The batch form is its own verb, [`Request::TypeBatch`]. One verb
    /// answering two cardinalities under one payload key would be the envelope
    /// rule's only ambiguous case, so it is two verbs and each is mono-shaped.
    Type(TypeArgs),
    /// Several type-defs by `names`: an array of results, ONE per requested
    /// name, in request order, each the single-`name` result or null for an
    /// unresolved name. The detail-on-demand dual of the `types` summary, so a
    /// consumer drilling into several summary entries pays one round trip.
    TypeBatch(TypeBatchArgs),
    /// Instances whose effective closure contains the named type, across all
    /// origins (file / nested inline record / type-def meta). `origins` narrows
    /// the site kinds returned; absent means all.
    InstancesOf(InstancesOfArgs),
    /// The workspace's discovered cross-repo import set: one record per
    /// (importing repo, imported type identity), the fold-axis `::repo` types
    /// each repo authors. `scope: own` keeps only imports made by the user's own
    /// repos and drops the automatic `au.engine.*` builtin fold (see [`Scope`]).
    Imports {
        #[serde(default)]
        scope: Scope,
    },
    /// Every type-def across the workspace whose closure includes `base`, each
    /// as its owning repo's def with the owner repo. The type-level
    /// dual of `instances_of`. The base itself is excluded. `scope: own` keeps
    /// only subtypes owned by the user's own repos (see [`Scope`]).
    Subtypes {
        base: String,
        #[serde(default)]
        scope: Scope,
    },
    /// Knowledge-base-wide per-instance introspection: every resolved instance's
    /// closure, effective shape, collisions, and full value layer.
    Instances,
    /// Knowledge-base-wide implicit-identity candidate scan, grouped by file. Paged by
    /// `limit` / `offset` (over files); projected to a lightweight summary
    /// (file + candidate type names) by `summary`. See [`CandidatesArgs`].
    Candidates(CandidatesArgs),
    /// Counts over the candidate scan: totals plus a by-type file histogram
    /// ("N untyped files could claim type X"). The candidates dual of
    /// `diagnostic_counts`, over the full scan (no paging). Takes no args.
    CandidateCounts(CandidateCountsArgs),
    /// Per-type instance counts: a total plus a per-identity histogram, so a
    /// consumer renders a vocabulary-with-counts overview in one round-trip. The
    /// instances dual of `type_counts`, scoped by the same `repo` / `scope` axis
    /// applied to the SITE'S repo. Closure-inclusive: a count equals the length
    /// of the matching `instances_of` drill-in. An unknown repo → null.
    InstanceCounts(TypeCountsArgs),
    /// One instance's resolved view, with the full value layer. Null when the
    /// path is not a parsed instance.
    ///
    /// Named `instance` (singular) against the knowledge-base-wide `instances`, the same
    /// singular/plural pair as `type` / `types`. The former name, `resolved`,
    /// was a past participle in a catalog of nouns, and collided with this
    /// view's own `resolved` boolean field.
    Instance { path: String },
    /// Inbound reference edges to a file. Named against `references_out`, so
    /// the one axis reads as one pair; formerly `backlinks`.
    ReferencesIn { path: String },
    /// Outgoing references from a file's body, in source order.
    ReferencesOut { path: String },
    /// Reverse-by-target inert-pin lookup: every commit-pinned reference naming
    /// `target` whose source file contains an instance of `source_type`. A fold
    /// over the retained OUTBOUND pins, not a backlink — an inert pin forms no
    /// inbound edge. Both fields required; `source_type` scopes the source-file
    /// set to `instances_of(source_type)`, keeping the fold off the whole graph.
    /// No time awareness: a since-reused name returns EVERY pin naming it, the
    /// consumer windows. See [[spec - pinned references - a recorded resolved
    /// edge with an immutable past and an on-demand forward trace]].
    Pins { target: String, source_type: String },
    /// Per-commit metadata for a set of commits, member-aware, off the build
    /// path. Each `commits[]` entry names a commit and optionally the member
    /// (`repo`) whose store to resolve it in, defaulting to the entry repo. The
    /// join partner for `pins`: a consumer collects the distinct
    /// `{ commit, repo? }` set and calls this once. Positional, one record per
    /// input in order; an absent commit is `available: false`. See
    /// [[spec - engine-mediated git reads - member-aware commit metadata and file history over the object store]].
    CommitMeta { commits: Vec<CommitRef> },
    /// A file's commit stream, member-aware, off the build path. `path` is the
    /// file (relative to the entry repo, or to `repo`'s member root when given).
    /// Each row carries the commit, timestamp, author, subject, and a `status` /
    /// `from` that are git's OWN rename similarity heuristic, best-effort, not
    /// authoritative. A non-git member or an unknown path yields an empty stream.
    /// See [[spec - engine-mediated git reads - member-aware commit metadata and file history over the object store]].
    FileHistory {
        path: String,
        #[serde(default)]
        repo: Option<String>,
    },
    /// A bounded, newest-first commit stream merged across the workspace's
    /// working trees, member-aware, off the build path. `members` filters to a
    /// subset by member name (each mapped to its owning tree and deduped),
    /// default all trees. `limit` (count) and / or `since` (a git time window)
    /// bound it; with neither, a default cap applies. Each row is keyed by its
    /// working-tree `tree` root plus the `members` in it, and carries
    /// `author{name,email}`, `timestamp`, `subject`, `changed_files`, and
    /// `trailers`. See [[spec - recent-commits activity stream - a member-aware
    /// bounded commit stream with a reflog-watched append-only subscription]].
    RecentCommits {
        #[serde(default)]
        members: Option<Vec<String>>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        since: Option<String>,
    },
    /// A bounded N-hop reference-graph walk from `path`: the reachable subgraph
    /// of nodes plus the edges traversed. Direction and depth are arguments. See
    /// [`NeighborhoodArgs`] and [[spec - neighborhood read - a bounded n-hop
    /// reference walk returning a subgraph of files and addressable blocks]].
    Neighborhood(NeighborhoodArgs),
    /// Resolve a wikilink target to a file path; null when unresolved.
    /// `target` carries the wikilink fragment grammar, so an embedded `::repo`
    /// qualifier is honored. `origin` scopes resolution to the file the link
    /// appears in, the way `references_out` resolves a body link; absent
    /// `origin` resolves a `::repo` target against the named repo and a bare
    /// target scopelessly across every repo index.
    ResolveTarget {
        target: String,
        #[serde(default)]
        origin: Option<String>,
    },
    /// Resolve a `^block-id` inside a target file to its typed block; null
    /// when the target or block doesn't resolve. `origin` scopes the target's
    /// resolution as in `resolve_target`.
    ResolveBlockId {
        target: String,
        block_id: String,
        #[serde(default)]
        origin: Option<String>,
    },
    /// Resolve a `#head` anchor inside a target file to its heading; null
    /// when the target or anchor doesn't resolve. Matching per
    /// [[type reference::au-type-system]]: case-insensitive exact heading text, first
    /// match in document order. `origin` scopes the target's resolution as in
    /// `resolve_target`.
    ResolveAnchor {
        target: String,
        anchor: String,
        #[serde(default)]
        origin: Option<String>,
    },
    /// Every heading in a target file, the listing dual of `resolve_anchor`:
    /// what a `#anchor` fragment can address, rather than whether one address
    /// resolves. Null when the target doesn't resolve; an empty array for a
    /// resolved file carrying no headings. `origin` scopes the target's
    /// resolution as in `resolve_target`.
    Anchors {
        target: String,
        #[serde(default)]
        origin: Option<String>,
    },
    /// Every addressable id in a target file, the listing dual of
    /// `resolve_block_id`: what a `^block_id` fragment can address, rather
    /// than whether one address resolves. Every occurrence is listed,
    /// duplicates included. Null when the target doesn't resolve; an empty
    /// array for a resolved file carrying none. `origin` scopes the target's
    /// resolution as in `resolve_target`.
    BlockIds {
        target: String,
        #[serde(default)]
        origin: Option<String>,
    },
    /// The resolvable file set, what a `[[` wikilink target can address: every
    /// catalogued file, assets included, with the stem a bare `[[name]]`
    /// resolves by. The listing dual of `resolve_target`, and the read dual of
    /// the `files` subscription channel. See [`FilesArgs`].
    Files(FilesArgs),
    /// Direct entries (files and subdirectories) of a directory. Named
    /// `dir_entries`, not `children`, because `type_tree.nodes[].children`
    /// already means graph children in this same API; formerly `children`.
    DirEntries { dir: String },
    /// A file's parsed frontmatter as a JSON map; null for non-instances.
    Frontmatter { path: String },
    /// A file's source text, content hash, and pin anchor,
    /// `{ content, hash, commit }`; null when unreadable. `hash` is the
    /// guard-usable `expected_hash`; `commit` is HEAD of the owning repo (null
    /// when not a git working tree), the pin anchor for the returned bytes.
    Content { path: String },
    /// A file's typed highlight tokens, an ordered `(range, kind)` stream;
    /// null when the path is not a readable markdown file.
    SemanticTokens { path: String },
    /// Validate a transient JSON value against a named type-def, returning the
    /// diagnostics a file with that frontmatter would get. The value never
    /// touches disk; spans index a synthesized document, not the value.
    ///
    /// MULTI-FIT: one verdict per mounted identity the name denotes. An unknown
    /// name yields ONE null-identity verdict carrying `unknown-type-claim`, the
    /// verdict a file gets, so a miss fails closed.
    ValidateValue {
        type_name: String,
        value: serde_json::Value,
        /// Scope to one repo's identity; absent = every mounted repo owning the
        /// name. A `::repo` in `type_name` wins over this arg.
        #[serde(default)]
        repo: Option<String>,
    },
    /// The workspace's declared members: each member's name, absolute root, and
    /// whether it is scattered outside the workspace root or a subdir of it.
    /// Optional `repo` narrows to that one member, so `members({repo})` mirrors
    /// `overview({repo}).members`.
    Members(MembersArgs),
    /// A member's file-scope rules: the editable `.auignore` patterns plus the
    /// non-editable default excludes and hard floor, for one member (`repo`) or
    /// every member. `resolve` adds the boundary-level effect (pruned dirs,
    /// excluded files) at a bounded extra walk, reported at boundaries not
    /// contents. An unknown `repo` yields an empty member list. See
    /// [[spec - scope management surface - an ignores read and a set_ignores config mutation]].
    Ignores {
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        resolve: bool,
    },
    /// Which declared member owns a path: the member's name and absolute root;
    /// null when the path lies under no declared member.
    ResolveMember { path: String },
    /// The per-user device-global engine-schema files (`repos.yaml`,
    /// `workspaces.yaml`): each file's resolved path, content, and field-shape
    /// diagnostics against its hardwired `au.engine.*` def. These sit outside
    /// every knowledge base, so the diagnostics land here, not on the `diagnostics` read.
    /// An entry is null when its path cannot be resolved (no per-user config
    /// directory). See
    /// [[spec - engine-schema files - hardwired-schema files are first-class substrate nodes]].
    DeviceConfig,
    /// One CONSUMER config file under the scoped-config channel,
    /// `<scope>/<consumer>/config/<file>`: its resolved path, content, and
    /// field-shape diagnostics against a stamped `type`. Out-of-band by the verb,
    /// never a walked node, the shape `device_config` returns. Machine scope
    /// resolves under `~/.arsumbris/`; repo scope under a declared member's
    /// `.arsumbris/`. An unresolved `type` is stored-as-is with a
    /// `config-type-unresolved` advisory. See
    /// [[spec - scoped config channel - a config read and set_config mutation over scope, consumer, file, type]].
    Config(ConfigArgs),
    /// The top-level directories of every mounted member, each tagged with its
    /// owning repo. `repo` selects one member, absent spans all; `scope`
    /// filters by class (`own` keeps the user's editable repos). Formerly
    /// `top_level_graphs`, which was entry-scoped and misnamed — these are
    /// folders, and nothing about the typed graph is consulted.
    TopLevelDirs(TopLevelDirsArgs),
    /// The up-front orientation map: workspace shape (members, top-level
    /// graphs), vocabulary shape (type counts), health shape (diagnostic
    /// counts), and the hub-node ranking over the typed reference graph. One
    /// cheap read a consumer loads first, then drills through the other reads.
    /// See [[spec - orientation overview read - a computed up-front map with a
    /// hub ranking over the typed reference graph]].
    Overview(OverviewArgs),
    /// The resolved ancestor closure and effective field set of a type
    /// identity. A bare `name` conflates, so it answers one closure PER
    /// matching identity; `repo` (or a `::repo` in the name) scopes to one.
    /// See [[spec - cross-repo identity on the wire - a name conflates, a
    /// qualifier scopes to identity, every result carries owner and hash]].
    TypeClosure(TypeClosureArgs),
    /// The most-referenced files over the typed reference graph, the "start
    /// here" signal. Promoted out of `overview`, which carried the ranking as
    /// an unmirrored field: every other `overview` field is reproducible by one
    /// call to its own read, and this one had no read to call.
    Hubs(HubsArgs),
    /// The scalar whole-graph summary folded from the catalog + backlink index:
    /// connected-component count, orphan counts, degree histograms, and density.
    /// The "your knowledge base is 14 disconnected components" signal, a fact
    /// you QUERY, never a diagnostic that nags. `repo` / `scope` like `overview`,
    /// mirrored by `overview.graph_shape`. See [[spec - whole-graph reads - a
    /// scalar shape summary and a full node-edge payload folded from the
    /// backlink index]].
    GraphShape(GraphShapeArgs),
    /// The full whole-graph node+edge payload a force-directed visualization
    /// lays out: every content file as a node (isolated ones included), every
    /// resolved reference as an edge. `repo` / `scope` like `graph_shape`. The
    /// un-ranked, un-truncated sibling of `hubs`, and the global-unseeded
    /// complement of `neighborhood`. See [[spec - whole-graph reads - a scalar
    /// shape summary and a full node-edge payload folded from the backlink
    /// index]].
    LinkGraph(LinkGraphArgs),
    /// The type graph as a node+edge payload, the type-side sibling of
    /// `link_graph`: type-def nodes with `subtype` / `field-type` edges by
    /// default, `instance-of` (adding claiming-instance nodes) and `meta` edges
    /// opt-in via `edges`. `repo` / `scope` like `link_graph`. Drawn beside
    /// `link_graph` and merged by path. See [[spec - whole-graph reads - a
    /// scalar shape summary and a full node-edge payload folded from the
    /// backlink index]].
    TypeGraph(TypeGraphArgs),
    /// Simulate a deterministic mutation over an overlay of the current snapshot
    /// and report its product, the would-be file's type identities and
    /// diagnostics plus the per-file blast radius, WITHOUT writing disk or
    /// committing. The pre-tool gate surface: a mediator previews a pending write
    /// to know its product before it lands. `op` selects the mutation to
    /// simulate; its arguments ride alongside. Structural refusals and a
    /// non-mountable path fold into the result's `reject`, so this is always a
    /// read, never a mutation. See [[spec - mutation preview read - simulate a
    /// write over an overlay and report its product without committing]].
    PreviewMutation(PreviewMutationArgs),
    /// The engine and ref lifecycle states. Named `lifecycle`, not `ready`,
    /// because "ready" meant three things at once: the frame's can-I-answer
    /// flag, this read, and a state VALUE. The frame keeps the flag.
    Lifecycle,
    /// Control verb: ask the daemon to shut down. Acked, then the serving
    /// process unblocks and exits, removing the socket.
    Shutdown,
}

/// The op a `preview_mutation` simulates, tagged by `op`. Mirrors the v1
/// mutation catalog's `write_file` / `edit_file` / `delete_file`, reusing their
/// argument shapes so a consumer previews the exact call it would then mutate
/// with. Deterministic ops only; non-deterministic ones (`bash`, codegen) have
/// no meaningful product.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum PreviewMutationArgs {
    WriteFile {
        path: String,
        content: String,
        #[serde(default)]
        stamps: Vec<Stamp>,
    },
    EditFile {
        path: String,
        old_string: String,
        new_string: String,
        #[serde(default)]
        replace_all: bool,
        #[serde(default)]
        stamps: Vec<Stamp>,
    },
    DeleteFile {
        path: String,
    },
}

impl PreviewMutationArgs {
    /// Split into the target path, the engine-side op, and the stamp riders.
    fn into_parts(
        self,
    ) -> (
        String,
        crate::mutate::PreviewOp,
        Vec<crate::mutate::PreviewStamp>,
    ) {
        let to_stamps = |ss: Vec<Stamp>| {
            ss.into_iter()
                .map(|s| crate::mutate::PreviewStamp {
                    field: s.field,
                    record: s.record,
                    match_on: s.match_on,
                })
                .collect()
        };
        match self {
            PreviewMutationArgs::WriteFile {
                path,
                content,
                stamps,
            } => (
                path,
                crate::mutate::PreviewOp::Write { content },
                to_stamps(stamps),
            ),
            PreviewMutationArgs::EditFile {
                path,
                old_string,
                new_string,
                replace_all,
                stamps,
            } => (
                path,
                crate::mutate::PreviewOp::Edit {
                    old_string,
                    new_string,
                    replace_all,
                },
                to_stamps(stamps),
            ),
            PreviewMutationArgs::DeleteFile { path } => {
                (path, crate::mutate::PreviewOp::Delete, Vec::new())
            }
        }
    }
}

/// A mutation request, tagged by `mutate` — the wire's third verb, per
/// [[spec - mutation channel v1 - a closed primitive catalog through one
/// mediated path]]. The catalog is closed; unknown primitives and unknown args
/// are error frames.
#[derive(Debug, Deserialize)]
#[serde(tag = "mutate", rename_all = "snake_case")]
enum Mutation {
    /// Full-content write, parents created. `expected_hash` is the optional
    /// read-before-write guard.
    WriteFile(WriteFileArgs),
    /// Exact string replacement; the match is its own precondition.
    EditFile(EditFileArgs),
    /// Remove a file. `expected_hash` is the optional read-before-write guard.
    DeleteFile(DeleteFileArgs),
    /// Engine-assigned `^:` id on the addressable entity at a byte offset;
    /// returns the id and the `[[target^id]]` ref.
    AssignBlockId(AssignBlockIdArgs),
    /// Move a file to a new path, rewriting every inbound reference to follow,
    /// as one saga. Same-repo only; refuses type-def files (that is `rename_type`).
    Rename(RenameArgs),
    /// Extract the inline `^:id` record at a byte offset into its own file,
    /// leaving `[[newFile]]` in its place and rewriting every `[[host^id]]`
    /// referrer to `[[newFile]]`, as one saga.
    Promote(PromoteArgs),
    /// Fold a `[[file]]`-referenced file into a host referrer as a `^:id`
    /// record, rewrite the other referrers to it, and delete the file, as one
    /// saga. The dual of promote.
    Inline(InlineArgs),
    /// Rename an inline `^:id` record's block-id in its host, rewriting every
    /// `[[host^id]]` referrer to the new id, as one saga. The block-id sibling of
    /// `rename` (which moves a file); rides the same rewrite core.
    RenameBlockId(RenameBlockIdArgs),
    /// Rename a type-def. Moves its file (the name derives from the filename)
    /// and cascades both reference surfaces atomically: the type-name
    /// references in the owning repo (`type:` claims, `sealed:` branches, slot
    /// shapes, qualified keys `field{name}`, body `use:`, meta `type:`), and the wikilinks
    /// to the def file across the mounted set. The type-vocabulary sibling of
    /// `rename`; what `rename` refuses for a type-def file.
    RenameType(RenameTypeArgs),
    /// Replace a member's `.auignore` scope rules — a CONFIG-sort mutation, not a
    /// graph mutation. It edits which content enters the graph, so it MAY orphan
    /// references (they become advisory diagnostics); the strict no-dangling
    /// contract is deliberately relaxed for it. The one sanctioned write under
    /// `.arsumbris/`, and only to `<root>/.arsumbris/.auignore`. Validates the
    /// patterns up front (a malformed pattern rejects, nothing written), commits
    /// per mutation, and re-scopes. See
    /// [[spec - scope management surface - an ignores read and a set_ignores config mutation]].
    SetIgnores(SetIgnoresArgs),
    /// Register (write or update) one `{ name, remote, path }` entry in the
    /// per-user registry (`~/.arsumbris/au-engine/config/repos.yaml`) — a CONFIG-sort
    /// mutation over a DEVICE-GLOBAL file, so NO git commit (the file is outside
    /// every repo), unlike `set_ignores`. The consumer-driven bootstrap for an
    /// unmounted peer: a folder picker supplies the path, `register` records it,
    /// the rebuild mounts it. Refuses (a reject frame) when the name's identity
    /// disagrees (`dependency-identity-conflict`). See
    /// [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]].
    Register(RegisterArgs),
    /// Write one CONSUMER config file under the scoped-config channel,
    /// `<scope>/<consumer>/config/<file>` — a CONFIG-sort mutation, the write dual
    /// of the `config` read. A sanctioned bypass of the `.arsumbris/` write-guard,
    /// like `set_ignores`: path-safety (and the reserved `au-engine` owner segment)
    /// gate the two caller-controlled segments, the path is constructed directly.
    /// Repo scope commits per mutation through the saga; machine scope writes a
    /// device-global file with NO commit. The write injects `type: <declared>` so
    /// the file self-describes, and an `expected_hash` is a compare-and-set. See
    /// [[spec - scoped config channel - a config read and set_config mutation over scope, consumer, file, type]].
    SetConfig(SetConfigArgs),
    /// Patch a nested typed record's fields in place, keyed by the `instances_of`
    /// `field_path` locator. `type` re-types the record; other keys replace a
    /// scalar field or insert an absent one. A byte-splice, so comments survive.
    /// See [[spec - nested record edits - patch a record and append to a sequence by byte-splice, comments preserved]].
    EditRecord(EditRecordArgs),
    /// Append one element to a sequence field, keyed by the `field_path` locator.
    /// A byte-splice after the last element (or a seed into an empty `[]`), so
    /// existing elements and their comments stay byte-identical.
    AppendRecord(AppendRecordArgs),
}

/// One `field_path` segment on the wire: a field name (string) or a list index
/// (number). Untagged, so `["phases", 0]` deserializes directly. The
/// deserialize dual of [`wire::PathSeg`]; index is tried first so a number never
/// reads as the string form.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PathSegArg {
    Index(usize),
    Field(String),
}

impl From<PathSegArg> for au_core::PathSegment {
    fn from(seg: PathSegArg) -> Self {
        match seg {
            PathSegArg::Index(i) => au_core::PathSegment::Index(i),
            PathSegArg::Field(f) => au_core::PathSegment::Field(f),
        }
    }
}

/// The validation stance for a nested-record mutation. `advise` (default) lands
/// the write and surfaces diagnostics; `reject` refuses when the change raises
/// the file's validation-error count, writing nothing.
#[derive(Debug, Deserialize, Default, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum OnInvalid {
    #[default]
    Advise,
    Reject,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditRecordArgs {
    path: String,
    /// The `instances_of` locator to the record; empty addresses the file instance.
    field_path: Vec<PathSegArg>,
    /// A shallow map of field name to value. `type` re-types; `^` is rejected.
    patch: serde_json::Map<String, serde_json::Value>,
    expected_hash: Option<String>,
    #[serde(default)]
    on_invalid: OnInvalid,
    #[serde(default)]
    stamps: Vec<Stamp>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppendRecordArgs {
    path: String,
    /// The `instances_of` locator to the SEQUENCE-valued field.
    field_path: Vec<PathSegArg>,
    /// The element to append, a record or a scalar.
    value: serde_json::Value,
    expected_hash: Option<String>,
    #[serde(default)]
    on_invalid: OnInvalid,
    #[serde(default)]
    stamps: Vec<Stamp>,
}

/// One entry of a write verb's `stamps` list: idempotently ensure a caller-supplied
/// record into a named frontmatter list-field of the file the write touches,
/// folded into the write's own commit. A verb carries a LIST, applied in order,
/// each entry the same idempotent ensure. The engine treats it as OPAQUE. See
/// [[spec - stamp injection - a write rider idempotently ensures a frontmatter record folded into the write's commit]].
/// Every stamp targets the verb's primary file only; the spec's `path` (another
/// file in the write set) is deferred.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stamp {
    /// The frontmatter sequence key the record is ensured in.
    field: String,
    /// The record to ensure, an arbitrary structured value.
    record: serde_json::Value,
    /// Optional dedup predicate: path-within-element to value pairs. When some
    /// existing element carries every pair the stamp is a no-op; absent, the
    /// record is always appended. v1 keys are top-level element slots (a field,
    /// or `type` for the element's claim).
    match_on: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArgs {
    path: String,
    content: String,
    expected_hash: Option<String>,
    #[serde(default)]
    stamps: Vec<Stamp>,
    /// Mixins to ensure on the written file's `type:` claim, folded into this
    /// write's commit ([[spec - ensure-mixin write directive - a governed write ensures a type-claim mixin idempotently, folded into the write's own commit]]).
    #[serde(default)]
    ensure_mixins: Vec<String>,
    #[serde(default = "default_true")]
    ensure_mixins_strict: bool,
    /// Caller attribution trailers folded into this write's commit.
    #[serde(default)]
    attribution: Vec<TrailerInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditFileArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    stamps: Vec<Stamp>,
    #[serde(default)]
    ensure_mixins: Vec<String>,
    #[serde(default = "default_true")]
    ensure_mixins_strict: bool,
    /// Caller attribution trailers folded into this edit's commit.
    #[serde(default)]
    attribution: Vec<TrailerInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteFileArgs {
    path: String,
    /// Optional read-before-write guard: the content hash the caller last read.
    /// A mismatch rejects without deleting; absent deletes regardless.
    expected_hash: Option<String>,
    /// Caller attribution trailers folded into this delete's commit.
    #[serde(default)]
    attribution: Vec<TrailerInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssignBlockIdArgs {
    path: String,
    /// A byte offset inside the file, e.g. from a span the wire served.
    /// The engine resolves the enclosing addressable entity.
    at: usize,
    #[serde(default)]
    stamps: Vec<Stamp>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameTypeArgs {
    old_name: String,
    new_name: String,
    #[serde(default)]
    stamps: Vec<Stamp>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterArgs {
    /// The repo name to register. Must equal the declared `name` in the target's
    /// `.arsumbris/repo.yaml` (checked before writing).
    name: String,
    /// The local path where the repo sits on this machine, absolute.
    path: String,
    /// The repo's git remote, an optional recorded convenience. A disagreement
    /// with an existing entry's remote is a `dependency-identity-conflict`.
    remote: Option<String>,
}

/// The scope of a scoped-config `(scope, consumer, file, type)` request.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConfigScope {
    /// `~/.arsumbris/<consumer>/config/<file>`, the personal device tier. The
    /// type resolves in the served entry's graph.
    Machine,
    /// `<repo>/.arsumbris/<consumer>/config/<file>`, the committed repo tier. The
    /// type resolves in the owning member's graph.
    Repo,
}

/// The `config` read arguments. `type` is stamped onto the file's value (not read
/// from a `type:` key) and field-shape validated in the scope's graph.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigArgs {
    scope: ConfigScope,
    /// The owner segment, a namespacing convention (`host-app`, `agent-tools`). Path-safe
    /// and not the engine's own `au-engine`.
    consumer: String,
    /// The config filename under `<consumer>/config/`.
    file: String,
    #[serde(rename = "type")]
    type_name: String,
    /// Repo scope: the member root whose `.arsumbris/` holds the file, a declared
    /// member root (as `members` surfaces). Absent defaults to the served entry.
    /// Ignored at machine scope.
    #[serde(default)]
    root: Option<String>,
}

/// The `set_config` mutation arguments. Exactly one of `content` (the whole file)
/// or `edit` (one keyed record splice) is given. `type` is the DECLARED floor:
/// injected into the content when the file does not already self-describe with its
/// own `type:`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetConfigArgs {
    scope: ConfigScope,
    consumer: String,
    file: String,
    #[serde(rename = "type")]
    type_name: String,
    /// Repo scope: the member root whose `.arsumbris/` holds the file (a declared
    /// member root). Absent defaults to the served entry. Ignored at machine scope.
    #[serde(default)]
    root: Option<String>,
    /// The whole file content to write. Exactly one of `content` / `edit`.
    #[serde(default)]
    content: Option<String>,
    /// A single keyed-record splice over the existing file (comments and siblings
    /// preserved). Exactly one of `content` / `edit`.
    #[serde(default)]
    edit: Option<ConfigEditArgs>,
    /// Optional compare-and-set against the current file's content hash. A
    /// mismatch (or an absent file) rejects, nothing written.
    #[serde(default)]
    expected_hash: Option<String>,
}

/// The `set_config` `edit` sub-shape: patch one keyed record in place, reusing the
/// byte-splice core `edit_record` uses. There is no `on_invalid` gate: a config
/// write is advisory (never refused on a field-shape verdict), and the splice's
/// own structural self-check still rejects a change that would break the YAML, so
/// the file is never left broken.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigEditArgs {
    /// The locator to the record; empty addresses the file's top-level record.
    field_path: Vec<PathSegArg>,
    /// A shallow map of field name to scalar value. `type` re-types a record that
    /// has an explicit claim; `^` is rejected.
    patch: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetIgnoresArgs {
    /// The member root whose `.auignore` to write. Must name a declared member
    /// root exactly (the `ignores` read surfaces it).
    root: String,
    /// The full replacement pattern list, one per line. An empty list removes the
    /// file, reverting the member to the default excludes.
    patterns: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameArgs {
    path: String,
    /// The new path. v1 must resolve to the same repo as `path`.
    to: String,
    /// Optional stamps, folded into the RENAMED file (`to`) after the move, so a
    /// `{ type: file-change.rename, from }` record shares the rename's commit.
    #[serde(default)]
    stamps: Vec<Stamp>,
    /// Mixins to ensure on the renamed file (`to`), folded into the rename's commit.
    #[serde(default)]
    ensure_mixins: Vec<String>,
    #[serde(default = "default_true")]
    ensure_mixins_strict: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromoteArgs {
    path: String,
    /// A byte offset inside the inline record to extract; the engine resolves the
    /// innermost enclosing record, mirroring `assign_block_id`'s locator. The
    /// locator for a record with no `^:` id. Exactly one of `at` / `block_id`.
    at: Option<usize>,
    /// The record's `^:` id; the stable locator for a record that already carries
    /// one (the referenced case always does). Exactly one of `at` / `block_id`.
    block_id: Option<String>,
    /// The new file's path. v1 must resolve to the same repo as `path`.
    to: String,
    /// Optional stamps, folded into the newly-extracted file (`to`).
    #[serde(default)]
    stamps: Vec<Stamp>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InlineArgs {
    /// The file to fold in and delete.
    path: String,
    /// The host referrer that will hold the inlined `^:id` record. Must resolve
    /// to the same repo as `path`, and must reference `path`.
    into: String,
    /// A byte offset in `into` landing on the `[[path]]` reference to host the
    /// record. Required when `into` references `path` more than once; the sole
    /// reference is used when omitted.
    at: Option<usize>,
    /// Optional stamps, folded into the host file (`into`) the content is pulled into.
    #[serde(default)]
    stamps: Vec<Stamp>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameBlockIdArgs {
    /// The host file carrying the inline `^:id` record.
    path: String,
    /// The record's current `^:` block-id.
    block_id: String,
    /// The new block-id. Must match the block-id grammar and be free in `path`.
    to_block_id: String,
    /// Optional stamps, folded into the host file (`path`).
    #[serde(default)]
    stamps: Vec<Stamp>,
}

/// A subscription request, tagged by `subscribe`, symmetric with [`Request`].
///
/// The argless variants and the parametric `diagnostics` channel together
/// are the closed v1 catalog. The channel name on the wire follows the catalog,
/// not the Rust identifier, see [`Subscription::channel`].
#[derive(Debug, Deserialize)]
#[serde(tag = "subscribe", rename_all = "snake_case")]
enum Subscription {
    /// The ref's Deriving-to-Ready lifecycle. Renamed with the read.
    Lifecycle,
    /// The type-def introspection stream. Matches the `types` read.
    Types,
    /// The set of catalogued files.
    Files,
    /// Every rebuild.
    Changes,
    /// Diagnostics: whole-knowledge-base when no filter is given, scoped by
    /// [`DiagnosticsFilterArgs`] otherwise. Multiplexed per connection.
    Diagnostics(DiagnosticsFilterArgs),
    /// The whole-graph node+edge payload as the initial value, then a node/edge
    /// DELTA per rebuild, so an interactive layout patches in place rather than
    /// re-shipping the graph. Scoped by [`LinkGraphArgs`] (`repo` / `scope`),
    /// the same args as the `link_graph` read. See [[spec - whole-graph reads -
    /// a scalar shape summary and a full node-edge payload folded from the
    /// backlink index]].
    LinkGraph(LinkGraphArgs),
    /// The type graph as a node+edge payload as the initial value, then a node/
    /// edge DELTA per rebuild, so a layout patches in place. Scoped by
    /// [`TypeGraphArgs`] (`repo` / `scope` / `edges`), the same args as the
    /// `type_graph` read. The type-side sibling of the `link_graph`
    /// subscription. See [[spec - whole-graph reads - a scalar shape summary and
    /// a full node-edge payload folded from the backlink index]].
    TypeGraph(TypeGraphArgs),
    /// The seed page of `recent_commits` as the initial value, then a
    /// `commits-appeared` change event carrying each newly-appeared commit. An
    /// APPEND-ONLY stream over a reflog watcher (off the version signal), the
    /// live half of the git ACTIVITY view. Scoped by the same `members` /
    /// `limit` / `since` as the read. See [[spec - recent-commits activity stream
    /// - a member-aware bounded commit stream with a reflog-watched append-only
    /// subscription]].
    RecentCommits(RecentCommitsSubArgs),
}

impl Subscription {
    /// The channel name as it appears on the wire, echoed in the ack.
    fn channel(&self) -> &'static str {
        match self {
            Subscription::Lifecycle => "lifecycle",
            Subscription::Types => "types",
            Subscription::Files => "files",
            Subscription::Changes => "changes",
            Subscription::Diagnostics(_) => "diagnostics",
            Subscription::LinkGraph(_) => "link_graph",
            Subscription::TypeGraph(_) => "type_graph",
            Subscription::RecentCommits(_) => "recent_commits",
        }
    }
}

/// Args for the `overview` read.
///
/// `scope` defaults to **`own`**, deliberately unlike the four ENUMERATION
/// reads that default to `all`. Orientation is inherently first-person: the
/// question is "where am I", not "what is loaded". The mounted set is the
/// transitive dependency closure, so a workspace-wide default would bury the
/// user's own content under infrastructure they did not write, and the default
/// is the decision — an arg nobody passes changes nothing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverviewArgs {
    /// Absent spans every mounted member; present selects that one.
    #[serde(default)]
    repo: Option<String>,
    /// `own` by default, `all` when a `repo` is named. See [`resolved_scope`].
    #[serde(default)]
    scope: Option<Scope>,
}

/// WHY the actionability reads (`overview`, `top_level_dirs`, `hubs`,
/// `diagnostics`, `diagnostic_counts`) default to `own` while the vocabulary
/// reads default to `all`.
///
/// The axis is "what should I act on" versus "what exists":
/// - an ACTIONABILITY read answers about content the user can change, so a
///   read-only dependency's contribution is noise. These default to `own`.
/// - a VOCABULARY read (`types`, `subtypes`, `imports`, `type_counts`) answers
///   what exists to author AGAINST. You write `book::library`, so hiding peer
///   types by default would hide the thing you are using and make completion,
///   hover, and the type tree look broken. These stay `all`.
///
/// That leaves ONE deliberate gap: `overview.type_counts` is own-scoped while
/// `type_counts({})` is all-scoped. `overview` echoes its resolved `repo` /
/// `scope` precisely so a consumer drilling down passes them verbatim instead
/// of re-deriving a default it cannot see.
/// The `scope` an actionability read runs at, given what the caller said.
///
/// An explicit `scope` always wins and always means what it says, so the two
/// args compose (AND) exactly as written. Only the DEFAULT is contextual:
///
/// - no `repo`: default `own`, the actionable answer.
/// - a `repo`: default `all`, because the caller already named the member they
///   want. Defaulting to `own` there would silently return NOTHING whenever the
///   named member is a dependency — an explicit request answered with an empty
///   list because of an invisible default, which is the exact failure class the
///   scoped reads exist to avoid.
///
/// `repo: "base", scope: "own"` still legitimately yields nothing: that is the
/// caller stating both filters, and an empty intersection they asked for.
fn resolved_scope(scope: Option<Scope>, repo: &Option<String>) -> Scope {
    match scope {
        Some(explicit) => explicit,
        None if repo.is_some() => Scope::All,
        None => Scope::Own,
    }
}

/// Args for the `members` read. Optional `repo` narrows to one member, so
/// `overview({repo}).members` is reproducible by one `members` call — the mirror
/// property, closed for this field. `members` honors `repo` but NOT `scope`: it
/// is the TOPOLOGY field, and `members.editable` is how a consumer sees which
/// members `own` selected.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MembersArgs {
    #[serde(default)]
    repo: Option<String>,
}

/// One commit in a `commit_meta` request: the commit-ish, and optionally the
/// member (`repo`) whose store to resolve it in. `repo` absent defaults to the
/// entry repo, so a single-repo consumer omits it. Per-commit, not per-batch,
/// because a folded pin set spans members.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitRef {
    commit: String,
    #[serde(default)]
    repo: Option<String>,
}

/// One caller-supplied attribution trailer on a mutation, `{ key, value }`. The
/// engine folds it into the commit verbatim and never interprets it, so a
/// `session` / `span` stays a caller concept. Validated against the reserved
/// engine keys before it is written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrailerInput {
    key: String,
    value: String,
}

/// Args for the `top_level_dirs` read. A dedicated strict struct, not an inline
/// enum variant, so a typo'd arg is an `error` frame rather than a silently
/// different-scoped answer — a scope typo must not quietly change the result.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TopLevelDirsArgs {
    #[serde(default)]
    repo: Option<String>,
    /// `own` by default, `all` when a `repo` is named. See [`resolved_scope`].
    #[serde(default)]
    scope: Option<Scope>,
}

/// Args for the `instances_of` read. A dedicated strict struct so an unknown
/// arg — a mistyped `body` / `instance` flag especially — is an `error` frame,
/// not a silently-ignored key that answers with the fact absent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstancesOfArgs {
    #[serde(rename = "type")]
    type_name: String,
    #[serde(default)]
    origins: Option<Vec<wire::Origin>>,
    /// Splice each match's resolved view (the `instance` read's payload) onto
    /// its record, keyed by the match's FILE. Default false is the base record.
    /// Named for the fact it attaches, NOT `resolve`: `resolve` means the
    /// boundary-effect walk on `ignores`, and a consumer's own `resolve?` toggle
    /// misled them, so the value-layer fact is `instance`.
    #[serde(default)]
    instance: bool,
    /// Splice each match's markdown BODY (the prose after frontmatter) onto its
    /// record, keyed by the match's FILE. Default false. The one fact that reads
    /// disk, so it splices off the executor.
    #[serde(default)]
    body: bool,
}

/// Args for the `type_closure` read.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeClosureArgs {
    /// The type name, bare (`foo`, conflating across mounted repos) or
    /// qualified (`foo::repo`, that one identity).
    name: String,
    /// Scope to one repo's identity. Absent = any-mounted, the multi-fit
    /// answer. A `::repo` in `name` says the same thing and wins.
    #[serde(default)]
    repo: Option<String>,
}

/// Args for the `hubs` read: the same `repo` / `scope` pair as `overview`,
/// plus paging.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HubsArgs {
    #[serde(default)]
    repo: Option<String>,
    /// `own` by default, `all` when a `repo` is named. See [`resolved_scope`].
    #[serde(default)]
    scope: Option<Scope>,
    /// Page size. Absent is the engine's `HUB_LIMIT`, the same bound
    /// `overview.hubs` carries, so the field mirrors an argless call.
    #[serde(default)]
    limit: Option<usize>,
    /// Page start, absent is 0.
    #[serde(default)]
    offset: Option<usize>,
}

/// Args for the `graph_shape` read: the same `repo` / `scope` pair as
/// `overview`, plus `orphan_paths`. The scalar whole-graph summary always
/// reports orphan COUNTS; `orphan_paths` opts into the unbounded path list, off
/// by default so a fragmented corpus's thousands of orphans stay behind a flag.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphShapeArgs {
    #[serde(default)]
    repo: Option<String>,
    /// `own` by default, `all` when a `repo` is named. See [`resolved_scope`].
    #[serde(default)]
    scope: Option<Scope>,
    /// Include the orphan file paths, each tagged `no_inbound` or `isolated`.
    /// Default false is counts only.
    #[serde(default)]
    orphan_paths: bool,
}

/// Args for the `link_graph` read: the same `repo` / `scope` pair as
/// `graph_shape`. Richer server-side filters (edge kinds, type filters, path
/// prefix) are a deferred follow-on, so the strict struct rejects them loudly
/// today rather than answering a differently-filtered graph.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkGraphArgs {
    #[serde(default)]
    repo: Option<String>,
    /// `own` by default, `all` when a `repo` is named. See [`resolved_scope`].
    #[serde(default)]
    scope: Option<Scope>,
}

/// Args for the `type_graph` read: the same `repo` / `scope` pair as
/// `link_graph`, plus `edges`, the edge-class selection. Absent `edges` is the
/// type-to-type backbone (`subtype` + `field-type`); `instance-of` and `meta`
/// are opt-in. An unknown edge class fails deserialization (an error frame).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeGraphArgs {
    #[serde(default)]
    repo: Option<String>,
    /// `own` by default, `all` when a `repo` is named. See [`resolved_scope`].
    #[serde(default)]
    scope: Option<Scope>,
    /// The edge classes to include; absent is `subtype` + `field-type`.
    #[serde(default)]
    edges: Option<Vec<EdgeClass>>,
}

/// The walk direction wire value. An unknown value fails deserialization, so a
/// typo is a malformed-read error frame, never a silent default.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum WireDirection {
    #[default]
    Out,
    In,
    Both,
}

impl From<WireDirection> for crate::neighborhood::Direction {
    fn from(d: WireDirection) -> Self {
        match d {
            WireDirection::Out => crate::neighborhood::Direction::Out,
            WireDirection::In => crate::neighborhood::Direction::In,
            WireDirection::Both => crate::neighborhood::Direction::Both,
        }
    }
}

/// Args for the `neighborhood` read. `path` is required; the rest default. An
/// unknown arg is a malformed-read error frame (`deny_unknown_fields`), and an
/// unknown `direction` value likewise. The `kinds`-required-past-depth-1 rule is
/// cross-field, so it is checked in the handler, not here.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NeighborhoodArgs {
    /// The seed file, always a file node.
    path: String,
    /// `out` (default) / `in` / `both`.
    #[serde(default)]
    direction: WireDirection,
    /// Maximum hop count; default 1 (the seed plus its direct targets).
    #[serde(default = "default_depth")]
    depth: usize,
    /// The edge-kind include set, any of `navigational` / `contributing` /
    /// `field`. Absent means all kinds at depth 1; REQUIRED past depth 1.
    #[serde(default)]
    kinds: Option<Vec<String>>,
    /// `all` (default; a `path` is a location pin) or `own` (prune at the repo
    /// boundary, a dependency target reported but not expanded).
    #[serde(default)]
    scope: Scope,
    /// The node cap; absent is the engine's [`NEIGHBORHOOD_MAX_NODES`]. The seed
    /// counts, so `max_nodes` 1 returns the seed alone.
    #[serde(default)]
    max_nodes: Option<usize>,
    /// Splice each node's `content`: a file node's whole-file
    /// `{ text, hash, commit }` from the working tree, a block node's span slice
    /// (`{ text, hash: null, commit: null }`, a block is not a git object).
    #[serde(default)]
    content: bool,
    /// Splice each FILE node's markdown `body` (the prose after frontmatter).
    #[serde(default)]
    body: bool,
    /// Splice each FILE node's resolved `instance` view; null for a plain note.
    #[serde(default)]
    instance: bool,
}

fn default_depth() -> usize {
    1
}

/// The `scope` arg on the workspace-wide type reads. `all` (default) surfaces
/// every mounted repo's vocabulary as before; `own` keeps only the user's own
/// primary, editable repos, hiding every dependency and the `au.engine.*`
/// builtin. See [`wire::TypeScope`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Scope {
    #[default]
    All,
    Own,
}

impl Scope {
    fn own_only(self) -> bool {
        self == Scope::Own
    }
}

/// Args for the `types` read. All optional. Unknown args are rejected loudly
/// rather than silently ignored, matching the diagnostics filters, per the
/// silent-drops-are-worse-than-rejection principle.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TypesArgs {
    /// Absent = the workspace-wide owner-deduped read. Present = scope to one
    /// member's whole graph (its own defs). An unknown repo → null.
    #[serde(default)]
    repo: Option<String>,
    /// Project each entry to the lightweight summary (`repo` / `name` / `hash`
    /// / `parents` / `sealed` / `doc`) instead of the full def.
    /// Default false = full detail. Detail-on-demand is the single `type` read.
    #[serde(default)]
    summary: bool,
    /// `all` (default) or `own` (only the user's own primary/editable repos).
    #[serde(default)]
    scope: Scope,
    /// Page size: at most this many entries after `offset`, over the name-sorted
    /// set. Absent returns the whole set.
    limit: Option<usize>,
    /// Page start: skip this many entries. Absent is 0.
    offset: Option<usize>,
}

/// Args for the `files` read. All optional, unknown args rejected loudly.
///
/// `scope` defaults to `all`, NOT the `own` an actionability read takes: a
/// wikilink into a dependency is legal (only a TYPE crossing gates on a
/// declared dep), so the resolvable target set is what exists to link against.
/// Defaulting to `own` would hide targets the validator accepts.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesArgs {
    /// Absent = every mounted member. Present = only this member's files.
    #[serde(default)]
    repo: Option<String>,
    /// `all` (default) or `own` (only the user's own editable repos).
    #[serde(default)]
    scope: Scope,
    /// Page size: at most this many entries after `offset`, over the
    /// path-sorted set. Absent returns the whole catalogue.
    limit: Option<usize>,
    /// Page start: skip this many entries. Absent is 0.
    offset: Option<usize>,
}

/// Args for the `type_counts` read: the same `repo` scope as `types`, but over
/// the FULL set, so no `summary` / `limit` / `offset`. Those are rejected as
/// unknown args (meaningless for a count), per silent-drops-are-worse.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeCountsArgs {
    #[serde(default)]
    repo: Option<String>,
    /// `all` (default) or `own` (only the user's own primary/editable repos).
    #[serde(default)]
    scope: Scope,
}

/// Args for the `type` read. `deny_unknown_fields` keeps the documented
/// strictness: an unknown arg is an `error` frame, never a silent result. A
/// missing `name` fails to deserialize, and a stray `names` is rejected as
/// unknown here — the two selectors are separate verbs now, so the former
/// both-present / neither-present validation is structural rather than a
/// `TryFrom`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeArgs {
    /// Bare (`foo`) or `::repo`-qualified (`foo::repo`), the authored form.
    name: String,
}

/// Args for the `type_batch` read, the batch dual of [`TypeArgs`]. Same
/// strictness.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeBatchArgs {
    /// Each bare or `::repo`-qualified; results come back order-matched.
    names: Vec<String>,
}

/// Args for the `candidates` read. All optional. The scan is knowledge-base-wide, so
/// there is no `repo` scope. Unknown args are rejected loudly, matching the
/// other read args.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidatesArgs {
    /// Project each file to the lightweight summary (file + candidate type
    /// names) instead of full per-candidate detail. Default false = full.
    #[serde(default)]
    summary: bool,
    /// Page size: at most this many files after `offset`, in the catalog's
    /// sorted order. Absent returns every scanned file.
    limit: Option<usize>,
    /// Page start: skip this many files. Absent is 0.
    offset: Option<usize>,
}

/// Args for the `candidate_counts` read. It takes none (the scan is knowledge-base-wide
/// and counts are over the full set), so any arg is rejected loudly, matching
/// the other reads' strictness.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateCountsArgs {}

/// The diagnostics filters, shared by the `diagnostics` read and channel.
/// All optional, all compose (AND). Unknown args are rejected loudly rather
/// than silently ignored — a typo'd filter must not return the unfiltered
/// set, per the silent-drops-are-worse-than-rejection principle.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticsFilterArgs {
    /// Keep diagnostics in exactly this file, repo-relative or absolute.
    path: Option<String>,
    /// Keep diagnostics in files under this directory path, matched on
    /// whole path components.
    path_prefix: Option<String>,
    /// Keep diagnostics of exactly this severity.
    severity: Option<Severity>,
    /// Keep diagnostics carrying exactly this code.
    code: Option<String>,
    /// Page size for the `diagnostics` read: at most this many entries, after
    /// `offset`. Absent returns the whole filtered set. Read-only: the
    /// subscription channel and `diagnostic_counts` operate over the full set
    /// and ignore paging.
    limit: Option<usize>,
    /// Page start for the `diagnostics` read: skip this many filtered entries
    /// before the page. Absent is 0.
    offset: Option<usize>,
    /// Keep diagnostics owned by exactly this member. Absent spans every
    /// mounted member, the `repo` meaning every read shares.
    repo: Option<String>,
    /// Keep diagnostics owned by the user's own repos (`own`, the DEFAULT) or
    /// by every mounted member (`all`). Orthogonal to `repo`, which selects
    /// WHICH member.
    ///
    /// Defaults to `own` because diagnostics are a WORKLIST, not an inventory:
    /// a validation error inside a read-only dependency is not something the
    /// user can act on, so it is noise in the default answer. The
    /// workspace-health codes (`peer-unmounted`, `edit-member-unmounted`,
    /// `dependency-cache-miss`) are anchored at the DECLARING repo's manifest,
    /// so they stay visible under `own` — nothing actionable is hidden.
    ///
    /// Defaults to `all` when ANY location pin is present — `repo`, `path`, or
    /// `path_prefix`. See [`Self::resolve`].
    #[serde(default)]
    scope: Option<Scope>,
}

impl DiagnosticsFilterArgs {
    /// Resolve against the knowledge base root: path args absolutize the way every
    /// other read's path arg does.
    fn resolve(self, root: &Path) -> DiagnosticsScope {
        // A LOCATION PIN — a named repo, file, or subtree — is an explicit
        // narrowing, so it defaults `scope` to `all`, the same guard `repo`
        // gets in `resolved_scope`. A `path` naming one file is at least as
        // explicit as a `repo`: asking for the file the user is looking at must
        // not empty out just because that file lives in a read-only dependency
        // (own-scoped, `owner_matches` would drop it). Only the UNPINNED
        // "give me my problems" case keeps the `own` worklist default.
        let pinned = self.repo.is_some() || self.path.is_some() || self.path_prefix.is_some();
        let own_only = match self.scope {
            Some(explicit) => explicit.own_only(),
            None => !pinned,
        };
        DiagnosticsScope {
            path: self.path.map(|p| abs_arg(root, &p)),
            path_prefix: self.path_prefix.map(|p| abs_arg(root, &p)),
            severity: self.severity,
            code: self.code,
            scope: wire::TypeScope::new(own_only),
            repo: self.repo,
        }
    }
}

/// The resolved diagnostics scope, the one filter predicate behind the
/// `diagnostics` read and channel.
struct DiagnosticsScope {
    path: Option<PathBuf>,
    path_prefix: Option<PathBuf>,
    severity: Option<Severity>,
    code: Option<String>,
    repo: Option<String>,
    scope: wire::TypeScope,
}

impl Default for DiagnosticsScope {
    fn default() -> Self {
        Self {
            path: None,
            path_prefix: None,
            severity: None,
            code: None,
            repo: None,
            scope: wire::TypeScope::all(),
        }
    }
}

impl DiagnosticsScope {
    /// Whether the repo axis is engaged at all, so the common unfiltered case
    /// skips the per-diagnostic owner resolution entirely.
    fn filters_by_repo(&self) -> bool {
        self.repo.is_some() || self.scope.own_only()
    }

    /// Whether a diagnostic's owning member passes the `repo` / `scope` pair.
    ///
    /// Ownership is the member owning the diagnostic's `span.file`, resolved by
    /// `repo_of`, which picks the DEEPEST containing repo — so a diagnostic
    /// inside a nested member attributes to that member, not to its container.
    ///
    /// An UNOWNED diagnostic is believed impossible: every knowledge-base-stream
    /// diagnostic is anchored either at a file the walk catalogued or at a
    /// mounted member's manifest (`peer-unmounted`, the likeliest candidate,
    /// anchors at the DECLARING repo's `repo.yaml`), and the device-global
    /// config diagnostics ride inside the `device_config` read rather than this
    /// stream. So this is a defensive branch, not a documented behaviour to
    /// code against. It asserts in debug rather than silently dropping: if some
    /// future emitter does produce one, we find out instead of quietly
    /// filtering it out of a scoped answer.
    fn owner_matches(&self, v: &KnowledgeBase, d: &Diagnostic) -> bool {
        if !self.filters_by_repo() {
            return true;
        }
        let Some(owner) = v.repos.repo_of(&d.span.file) else {
            debug_assert!(
                false,
                "diagnostic {} at {} belongs to no mounted member; scoped \
                 diagnostics would silently drop it",
                d.code.as_str(),
                d.span.file.display()
            );
            return false;
        };
        if let Some(want) = self.repo.as_deref() {
            if owner.name.as_str() != want {
                return false;
            }
        }
        self.scope.includes_repo(v, owner)
    }

    fn matches(&self, v: &KnowledgeBase, d: &Diagnostic) -> bool {
        self.path.as_deref().map_or(true, |p| d.span.file == p)
            && self
                .path_prefix
                .as_deref()
                .map_or(true, |p| d.span.file.starts_with(p))
            && self.severity.map_or(true, |s| d.severity == s)
            && self.code.as_deref().map_or(true, |c| d.code.as_str() == c)
            && self.owner_matches(v, d)
    }
}

/// The diagnostics in a scope, over the served stream.
///
/// A `path` or `path_prefix` scope narrows to a contiguous key range first, so
/// a per-file or per-directory read is O(log n + k), not a whole-stream scan.
/// The full `matches` predicate still runs over the narrowed set, so any
/// `severity` / `code` filter rides on top with identical semantics. A scope
/// with no path narrowing (unfiltered, or severity/code only) legitimately
/// spans the whole stream.
fn scoped_diagnostics<'a>(
    v: &'a KnowledgeBase,
    scope: &'a DiagnosticsScope,
) -> Box<dyn Iterator<Item = &'a Diagnostic> + 'a> {
    if let Some(path) = scope.path.as_deref() {
        Box::new(
            v.diagnostics_for_file(path)
                .filter(move |d| scope.matches(v, d)),
        )
    } else if let Some(prefix) = scope.path_prefix.as_deref() {
        Box::new(
            v.diagnostics_under(prefix)
                .filter(move |d| scope.matches(v, d)),
        )
    } else {
        Box::new(v.diagnostics().filter(move |d| scope.matches(v, d)))
    }
}

/// The `diagnostic_counts` read result: the filtered set's total, plus
/// breakdowns by severity and by code, each name-sorted (BTreeMap).
#[derive(Debug, Serialize)]
struct DiagnosticCountsView {
    total: usize,
    by_severity: std::collections::BTreeMap<String, usize>,
    by_code: std::collections::BTreeMap<String, usize>,
}

/// Count the scoped diagnostics into totals by severity and by code, the
/// `diagnostic_counts` read's body. Shared with the `overview` read, which
/// counts the whole knowledge base (an empty scope).
fn diagnostic_counts_view(v: &KnowledgeBase, scope: &DiagnosticsScope) -> DiagnosticCountsView {
    let mut total = 0usize;
    let mut by_severity: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut by_code: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for d in scoped_diagnostics(v, scope) {
        total += 1;
        *by_severity
            .entry(severity_str(d.severity).to_string())
            .or_default() += 1;
        *by_code.entry(d.code.as_str().to_string()).or_default() += 1;
    }
    DiagnosticCountsView {
        total,
        by_severity,
        by_code,
    }
}

/// The wire string for a severity, the same lowercase form `Severity`
/// serializes to and the `severity` filter accepts.
fn severity_str(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Drift => "drift",
        Severity::Warning => "warning",
        Severity::Hint => "hint",
    }
}

/// A `response` frame: the reply to a `read`. `ready` is false while the ref is
/// Deriving; `version` and `result` are present only when ready.
#[derive(Debug, Serialize)]
struct Response {
    #[serde(rename = "type")]
    kind: &'static str,
    schema_version: u32,
    /// The request's `id`, echoed so the client settles by id. Omitted when the
    /// request carried none. Stamped on the way out, after the handler builds
    /// the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<serde_json::Value>,
    ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    /// The human-facing reason on an `error`-kind response. `None` on a normal
    /// response. Present when a read's ARGUMENTS are semantically invalid past
    /// what serde can reject (a cross-field rule like "kinds required past depth
    /// 1"), matching the `error` field a malformed-frame error carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Response {
    fn not_ready() -> Self {
        Response {
            kind: "response",
            schema_version: SCHEMA_VERSION,
            id: None,
            ready: false,
            version: None,
            result: None,
            error: None,
        }
    }

    fn ready(version: u64, result: serde_json::Value) -> Self {
        Response {
            kind: "response",
            schema_version: SCHEMA_VERSION,
            id: None,
            ready: true,
            version: Some(version),
            result: Some(result),
            error: None,
        }
    }

    /// An `error`-kind response for a semantically-invalid read argument, the
    /// handler-level analogue of the parse-layer `error_frame`. A cross-field
    /// rule (`kinds` required past depth 1) that serde cannot express is
    /// checked in the handler and rejected here rather than answered.
    fn invalid(message: impl Into<String>) -> Self {
        Response {
            kind: "error",
            schema_version: SCHEMA_VERSION,
            id: None,
            ready: false,
            version: None,
            result: None,
            error: Some(message.into()),
        }
    }

    /// Stamp the request's `id` onto this response so the client settles by id.
    fn with_id(mut self, id: Option<serde_json::Value>) -> Self {
        self.id = id;
        self
    }
}

/// Knowledge-base-wide per-instance introspection. `entries` is empty when the build
/// aborted at load; `aborted_at_load` distinguishes that from a knowledge base with no
/// instances. Per-file diagnostics ride the `diagnostics` read, not duplicated
/// here.
#[derive(Debug, Serialize)]
struct InstancesView {
    count: usize,
    aborted_at_load: bool,
    /// The payload, keyed by the read's own name. `count` and
    /// `aborted_at_load` beside it are envelope metadata ABOUT the payload.
    instances: Vec<wire::InstanceIntrospection>,
}

/// Knowledge-base-wide implicit-identity candidate scan, grouped by file. `aborted_at_load`
/// distinguishes a skipped scan (broken vocabulary) from an empty one. `files`
/// is full detail or the lightweight summary, paged by [`CandidatesArgs`].
#[derive(Debug, Serialize)]
struct CandidatesView {
    aborted_at_load: bool,
    /// The payload, keyed by the read's own name; `aborted_at_load` beside it
    /// is envelope metadata.
    candidates: wire::WireCandidateFiles,
}

/// One instance's resolved view, coherent at one version. `resolved` is false
/// when the claim doesn't resolve (no effective shape); `closure` is then
/// empty. The value layer (`effective_values`, `section_presence`,
/// `body_events`) carries full contribution provenance, serving `ProvenancePort`
/// from the same coherent read; it is present even for an unresolved claim,
/// since frontmatter and body contributions don't need the graph.
#[derive(Debug, Serialize)]
struct ResolvedView {
    resolved: bool,
    claim: Vec<String>,
    closure: Vec<String>,
    candidates: Vec<CandidateDto>,
    effective_values: Vec<wire::FieldValuesEntry>,
    section_presence: Option<Vec<wire::SectionPresenceEntry>>,
    body_events: Option<Vec<wire::BodyEventIntrospection>>,
    /// Addressable inline records ([[type block-id::au-type-system]]); omitted when none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    record_block_ids: Vec<wire::RecordBlockIdIntrospection>,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Serialize)]
struct CandidateDto {
    type_name: String,
    /// RFC 6901 JSON pointer to the scope inside the file (`""` = top level).
    inline_path: String,
}

#[derive(Debug, Serialize)]
struct BacklinkDto {
    source: String,
    /// The source's repo, present only when the inbound edge crosses a repo
    /// boundary (the source is in a different repo than the target). Omitted for
    /// a repo-local edge. Saves the consumer a `resolve_member` round-trip on the
    /// absolute `source` path to recover cross-repo provenance.
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    slot: Option<String>,
    surface: String,
    /// The inbound edge kind, `navigational` / `contributing` / `field`. Derived
    /// from `surface` and `slot`, so it never disagrees with them. `field` is a
    /// frontmatter-surface edge; the typed/untyped split `references_out` makes
    /// (`field-reference` / `field-string-wikilink` / `unknown`) is deferred
    /// inbound, a later refinement of `field`. See [`inbound_kind`].
    kind: &'static str,
    span_start: usize,
    span_end: usize,
    /// Line/column rendering of the span, in the `source` file.
    #[serde(skip_serializing_if = "Option::is_none")]
    line_col: Option<au_diagnostics::LineColRange>,
    /// The TARGET block-id fragment with its mode: `{ id, referent }`. `referent`
    /// is `true` for a `^^id` block-referent, `false` for a bare `^id` anchor.
    block_id: Option<au_references::BlockId>,
    /// The `^:` id of the source record the reference lives in, when the
    /// edge originates inside an inline record — renders as
    /// `[[<source>^<source_block_id>]]`. Omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    source_block_id: Option<String>,
}

/// A node reference in a `neighborhood` edge: the file, plus the `^^`-addressed
/// block id when the endpoint is a block node.
#[derive(Debug, Serialize)]
struct NodeRefDto {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_id: Option<String>,
}

/// One reached node in the `neighborhood` subgraph.
#[derive(Debug, Serialize)]
struct NeighborhoodNodeDto {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_id: Option<String>,
    depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    file_kind: &'static str,
    /// Byte length a consumer costs a `content` fetch by: a file node's whole-file
    /// length, a block node's span length. `null` for an unread asset or a block
    /// whose `^^` id did not resolve.
    bytes: Option<usize>,
    /// Byte length a consumer costs a `body` fetch by: a file node's prose-body
    /// length, a block node's span length. `null` for a pure-YAML / type-def
    /// file, or a block whose id did not resolve.
    body_bytes: Option<usize>,
}

/// One traversed edge in the `neighborhood` subgraph, stored natural-direction.
/// The `references_out` fields, populated from whichever direction discovered
/// the edge: an inbound-discovered edge carries no `repo` / `commit` / `anchor`
/// (the backlink index dropped the link).
#[derive(Debug, Serialize)]
struct NeighborhoodEdgeDto {
    from: NodeRefDto,
    /// `null` for a dangling edge (an outbound link resolving to nothing), or a
    /// scope- / budget-excluded target that is not in `nodes`.
    to: Option<NodeRefDto>,
    kind: &'static str,
    surface: &'static str,
    span_start: usize,
    span_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    line_col: Option<au_diagnostics::LineColRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_block_id: Option<String>,
    /// The block-id fragment on the link, `{ id, referent }`; absent when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    block_id: Option<au_references::BlockId>,
    /// The `::repo` qualifier, present only on an outbound-discovered crossing.
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    /// The `@commit` pin, present only on an outbound-discovered pinned link.
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    /// The `#anchor` fragment, present only on an outbound-discovered link.
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor: Option<String>,
}

/// A node cut by `max_nodes`, named so a consumer can surface or re-fetch it.
#[derive(Debug, Serialize)]
struct DroppedNodeDto {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_id: Option<String>,
    depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
}

/// The `neighborhood` read payload: the reachable subgraph plus the truncation
/// report.
#[derive(Debug, Serialize)]
struct NeighborhoodDto {
    nodes: Vec<NeighborhoodNodeDto>,
    edges: Vec<NeighborhoodEdgeDto>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated_at_depth: Option<usize>,
    dropped: Vec<DroppedNodeDto>,
}

/// One outgoing body wikilink: the bare target, the resolved file path (null
/// when it doesn't resolve), the source span (bytes plus line/column), and
/// the fragment parts. Serves `LinkGraphPort.getOutgoing`.
#[derive(Debug, Serialize)]
struct OutgoingRefDto {
    target: String,
    resolved: Option<String>,
    span: wire::SpanRange,
    /// The `::repo` qualifier when the link crosses a repo boundary; absent for
    /// an unqualified link. `resolved` then points into that repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    /// The `@commit` pin, the commit-ish a pinned reference resolves against;
    /// absent for an unpinned link. Binds to `::repo`. The resolved-edge mirror
    /// of the `body_events.wikilink.parsed` `commit`, so the outgoing-edge set
    /// carries which edges are pinned without a per-file body scan.
    ///
    /// A commit-bearing edge is an INERT PIN: it is a snapshot into an immutable
    /// past, so `resolved` is null (a pin does not attach to a live file) and the
    /// edge is NEVER dangling, whatever its `kind`. A consumer MUST NOT read a
    /// pin's null `resolved` as a broken link — the `commit` presence is the
    /// signal. This holds for the named-target pin (`[[file::@sha]]`, an ordinary
    /// `kind` here) as well as the empty-target `commit-referent` kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor: Option<String>,
    /// The block-id fragment with its mode: `{ id, referent }`. `referent` is
    /// `true` for a `^^id` block-referent (the block's value fills the slot),
    /// `false` for a bare `^id` navigational anchor (the file is the referent).
    #[serde(skip_serializing_if = "Option::is_none")]
    block_id: Option<au_references::BlockId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    /// The `^:` id of the enclosing inline record this edge originates from,
    /// when it does. Mirrors the inbound edge's field of the same name, so the
    /// two directions describe an edge the same way.
    #[serde(skip_serializing_if = "Option::is_none")]
    source_block_id: Option<String>,
    /// Which surface the edge sits on: `frontmatter` or `body`.
    surface: &'static str,
    /// What KIND of edge this is. Load-bearing, not cosmetic: the kind decides
    /// how a consumer should act on the edge and how severe a break is.
    ///
    /// The "dangling" reading below is OVERRIDDEN when `commit` is set: a
    /// commit-bearing edge is an inert pin, so its null `resolved` is expected,
    /// not a break, whatever this kind says. Check `commit` before treating any
    /// null `resolved` as dangling.
    ///
    /// - `field-reference` — a typed reference field value in frontmatter, a
    ///   slot that admits one (`myType*` / `file*` / `any*` / a reference
    ///   branch of a compound). The structural edge; validated and
    ///   closure-checked, and a dangling one is an error.
    /// - `field-string-wikilink` — a `[[...]]` in a frontmatter value whose
    ///   slot does NOT admit a reference, or one embedded in a longer string.
    ///   An intended-but-untyped pointer; navigational only.
    /// - `contributing` — a body `[[target:field]]`, which is both a link and a
    ///   data contribution. Following it says where a field value came from.
    /// - `navigational` — a body prose `[[...]]` with no attribution. A hint;
    ///   a dangling one is a warning.
    /// - `commit-referent` — a commit-only reference (`[[::@sha]]` /
    ///   `[[::repo@sha]]`) that names a COMMIT, not a file. `resolved` is null and
    ///   `commit` is set; the commit stays readable because git history is
    ///   append-only, and the edge is NEVER dangling, so a consumer must not treat
    ///   its null `resolved` as a broken link. Surface-independent, settled by the
    ///   link's own syntax.
    /// - `unknown` — a WHOLE-VALUE frontmatter wikilink on a TYPED instance
    ///   whose slot could not be resolved, so the engine cannot say which of the
    ///   first two it is.
    ///
    /// The frontmatter split is decided by the SLOT, never by the value's
    /// syntax — the same rule the value layer applies.
    ///
    /// `unknown` exists because the alternative is a LIE. Where a typed
    /// instance's effective shape is absent (its `type:` claim does not resolve,
    /// or its repo's vocabulary aborted) this used to report
    /// `field-string-wikilink`, which is documented above as a positive
    /// assertion that the slot does not admit a reference. Saying "I cannot
    /// tell" is the honest answer, and it is NARROW: it needs a `type:` intent
    /// the engine could not honor. A body edge is settled by the link's syntax,
    /// an embedded frontmatter link at the value level, and a plain NOTE is
    /// definitively untyped (no slot admits a reference, so `field-string-
    /// wikilink`) — none of those can be `unknown`.
    kind: &'static str,
}

/// One inert-pin record the `pins` reverse read returns: a commit-pinned
/// reference naming `target`, and where it sits. The inbound direction an
/// inert pin drops, recovered by folding the retained OUTBOUND pins, so every
/// field is source-side. Never a live edge, so there is no `resolved` — the
/// pin names a coordinate into an immutable past, not a current file.
#[derive(Debug, Serialize)]
struct PinRecordDto {
    /// The file holding the pin.
    source: String,
    /// The `^:` id of the enclosing inline record the pin sits in, when it does
    /// — a nested-record site (au-provenance's spans nest inside a ledger file).
    /// Absent for a top-level frontmatter or body pin.
    #[serde(skip_serializing_if = "Option::is_none")]
    source_block_id: Option<String>,
    /// The pin's byte span in `source`, plus derived line/column.
    span: wire::SpanRange,
    /// The field the pin fills, the innermost key for a nested value; a body
    /// `:field` attribution; or absent for an untyped prose pin. A consumer
    /// filters by field (au-provenance keeps `read`) without a second read.
    #[serde(skip_serializing_if = "Option::is_none")]
    slot: Option<String>,
    /// Which surface the pin sits on: `frontmatter` or `body`.
    surface: &'static str,
    /// The pinned target NAME, exactly as the pin recorded it. Not live-
    /// resolved: the read matches the recorded coordinate, never the graph.
    target: String,
    /// The `::repo` scope when the pin crosses a repo boundary; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    /// The pinned commit, always present — a pin is a commit-bearing reference.
    commit: String,
    /// The `^block-id` / `^^block-id` fragment on the pin, `{ id, referent }`;
    /// absent when the pin names no interior block.
    #[serde(skip_serializing_if = "Option::is_none")]
    block_id: Option<au_references::BlockId>,
}

/// A resolved `[[target^block-id]]` reference: the file carrying the typed
/// block, its `type:` claim, and the fenced block's byte span. Serves
/// `LinkGraphPort.resolveBlockId`.
#[derive(Debug, Serialize)]
struct ResolvedBlockDto {
    file_path: String,
    /// Empty for a navigational `marker` and for a claim-less record.
    type_claim: Vec<String>,
    span: wire::SpanRange,
    /// Which addressable entity the id reached ([[type block-id::au-type-system]]):
    /// `record` (inline `^:`), `typed_block` (`[:field]` fence), or
    /// `marker` (bare `^id` marker or untyped fence id) — navigational,
    /// never a typed-reference target.
    kind: &'static str,
}

/// A resolved `#head` anchor: the heading line. Navigational only,
/// anchors are never type-bearing ([[type reference::au-type-system]]).
#[derive(Debug, Serialize)]
struct ResolvedAnchorDto {
    file_path: String,
    span: wire::SpanRange,
}

/// One addressable heading in a file: what a `#anchor` fragment matches,
/// plus where it sits.
///
/// `file_path` is absent by design — it is the read's argument, so repeating
/// it per entry says nothing. What remains is `resolve_anchor`'s payload
/// (`span`) plus the key that selects it (`text`), so a consumer that lists
/// and then picks needs no second round trip.
#[derive(Debug, Serialize)]
struct AnchorDto {
    /// Exactly what a `#anchor` fragment matches: the heading text with its
    /// trailing `^id` marker excluded (the body scanner strips the marker
    /// before parsing the heading, so the text never carries one). Serving
    /// the matched form means a consumer inserts it verbatim and the link
    /// resolves.
    text: String,
    /// Heading depth, 1..=6. Carried because the scan already holds it, so
    /// an outline consumer needs no second read.
    level: u8,
    /// The heading LINE, the same span `resolve_anchor` returns.
    span: wire::SpanRange,
}

/// One addressable id in a file: what a `^block_id` fragment can reach.
///
/// `file_path` is absent for the same reason it is on [`AnchorDto`]: it is the
/// argument. What remains is `resolve_block_id`'s payload (`kind`,
/// `type_claim`, `span`) plus the key that selects it (`id`).
#[derive(Debug, Serialize)]
struct BlockIdDto {
    /// The bare id, without its `^` sigil, as `[[file^id]]` spells it.
    id: String,
    /// Which addressable entity carries it, `resolve_block_id`'s vocabulary
    /// verbatim ([[type block-id::au-type-system]]): `record` (inline `^:`), `typed_block`
    /// (`[:field]` fence), or `marker` (bare `^id` marker or untyped fence
    /// id).
    ///
    /// Typedness rides HERE, never a separate flag: a `^^` block-referent
    /// demands a typed value, satisfied by `record` and `typed_block` and not
    /// by `marker`. So a consumer completing `^^` filters on this field.
    kind: &'static str,
    /// The effective claim, explicit or slot-pinned. Empty for a `marker` and
    /// for a claim-less record.
    type_claim: Vec<String>,
    /// The addressed entity's span: the record's `^:` value, the whole fence,
    /// or the marker line. The same span `resolve_block_id` returns.
    span: wire::SpanRange,
}

/// One catalogued file: what a `[[` wikilink target can resolve to.
#[derive(Debug, Serialize)]
struct FileDto {
    /// The absolute path, the same address space every other read serves.
    path: String,
    /// The basename minus ONE extension, what a bare `[[name]]` resolves by
    /// ([[type reference::au-type-system]]): `x.session.yaml` stems to `x.session`. Served so
    /// no consumer re-derives the strip-one-extension rule, and the
    /// extensionless-match rule with it. Not unique: two files sharing a stem
    /// are an ambiguous bare target, which resolution reports and this listing
    /// does not.
    stem: String,
    /// The member owning the file; null when the path belongs to none.
    repo: Option<String>,
    /// `instance` / `type-def` / `note` / `asset`, the shared vocabulary. See
    /// [`crate::parse::served_file_kind`].
    kind: &'static str,
}

/// One semantic token: a source `range` plus its AU-semantic role. The
/// `kind`-specific fields flatten beside `range`, so a token is
/// `{ range, kind, ... }`. Spec [[spec - semantic tokens]].
#[derive(Debug, Serialize)]
struct SemanticToken {
    range: wire::SpanRange,
    #[serde(flatten)]
    kind: SemanticTokenKind,
}

/// The AU-semantic role of a [`SemanticToken`], a tagged union on `kind`.
/// The body-surface kinds (wikilink, block-id, anchor) serve notes and
/// typed instances alike; the value-layer kinds (field-value, type-claim,
/// typed-block) join in as the read grows.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum SemanticTokenKind {
    /// A body or frontmatter wikilink whose target resolves to a file.
    WikilinkResolved {
        target: String,
        /// The `::repo` qualifier when the link crosses a repo boundary.
        #[serde(skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
        resolved: String,
    },
    /// A wikilink whose target does not resolve.
    WikilinkBroken {
        target: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    /// A commit-pinned wikilink: a named pin (`[[file::@sha]]`) or the empty-target
    /// commit-referent (`[[::@sha]]` / `[[::repo@sha]]`). An inert coordinate into an
    /// immutable past, never re-resolved live and never dangling, so it is neither
    /// resolved nor broken. This ONE kind covers every inert pin (`is_inert_pin`),
    /// which is NOT symmetric with `references_out`: there only the empty-target
    /// commit-referent gets the `commit-referent` edge kind, while a named pin keeps
    /// its ordinary kind (`navigational` / `field-reference` / ...) and carries its
    /// inertness on the orthogonal `commit` / null-`resolved` fields instead.
    WikilinkPinned {
        target: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
        commit: String,
    },
    /// A typed scalar value, colored by its resolved type. `value_type` is
    /// the leaf's shape, a list element carrying the element shape. The type
    /// is the declared shape when the engine knows it (top-level, slot-
    /// pinned, or list-element), else inferred from the YAML literal for an
    /// extra or untyped-record field, always a bare primitive. The two are
    /// wire-identical by design; the declared-vs-extra distinction is a
    /// schema question, on the diagnostics / `type` reads.
    FieldValue {
        field: String,
        value_type: wire::WireShape,
    },
    /// A `type:` reference to a type-def, one per claimed name. Serves an
    /// instance identity claim, a type-def parent claim, and a `meta:`
    /// sub-region's `type:` alike.
    TypeClaim { name: String },
    /// An inline `[:field]` typed fence, spanning the whole fence. A
    /// container: its inner scalars emit their own `field-value` tokens.
    TypedBlock { field: String },
    /// An addressable id: a bare `^id` marker, a trailing fence id, or a
    /// `^:` inline-record id.
    BlockId { id: String },
    /// A heading, the target a `#anchor` fragment resolves to.
    Anchor { text: String },
    /// A type-def field's declared shape, a container over the whole shape
    /// expression. `value_type` is the parsed shape; null when the shape
    /// failed to parse. Encloses the type-ref / shape-builtin / enum-member
    /// leaves.
    FieldShape {
        field: String,
        value_type: Option<wire::WireShape>,
    },
    /// A navigable type-def name in a type-def's structure: a field-shape name
    /// (record / reference / inline base, compound operand, def-ref bound) or a
    /// `sealed:` branch.
    TypeRef { name: String },
    /// A built-in shape keyword: a primitive, `file`, `any`, or `type`.
    ShapeBuiltin { name: String },
    /// An inline enum literal in a field shape.
    EnumMember { value: String },
}

/// One direct child of a directory. Serves `RepoFilesPort.listChildren` and,
/// at the root, `TopLevelGraphsPort`.
#[derive(Debug, Serialize)]
struct DirEntryDto {
    path: String,
    name: String,
    kind: &'static str,
}

/// One top-level directory of one MOUNTED MEMBER, tagged with the member that
/// owns it. The repo tag is what makes a multi-member result readable, and it
/// lets a consumer regroup by member without a second read.
#[derive(Debug, Serialize)]
struct TopLevelDirDto {
    repo: String,
    name: String,
    path: String,
    /// The declared name of the mounted member ROOTED at this directory, when
    /// there is one; `None` for an ordinary content directory.
    ///
    /// A member may sit physically inside another repo (legal, though the usual
    /// topology places members elsewhere). Its folder then genuinely IS a
    /// directory of the containing repo, so it is reported for BOTH: once as
    /// the container's own top-level directory, and again as the member's own
    /// dirs under the member's tag. The overlap is honest, not double-counting
    /// to be filtered away.
    ///
    /// This field is what makes it legible. A consumer could derive it by
    /// joining `path` against every `members[].root`, but that pushes a
    /// path-equality comparison (canonicalization, symlinks, separators) onto
    /// every consumer for a fact known for free here. The member NAME rather
    /// than a bare flag, so the loop closes without a second read; a consumer
    /// wanting `root` / `role` / `editable` calls `members` once and joins by
    /// name, the same shape `instances_of.member` takes.
    ///
    /// Non-`None` only for a DECLARED member: the content walk is bounded at an
    /// undeclared nested repo's marker, so such a directory holds no catalogued
    /// file and never appears here at all.
    member: Option<String>,
}

/// A hub node in the `overview` read: a referenced file ranked by its inbound
/// reference count over the typed reference graph. `refs_structural` counts the
/// inbound edges filling a typed slot, `refs_navigational` the prose links; the
/// split is the neutral fact a consumer re-ranks by, there is no opaque score.
/// `kind` is the file's own role, `repo` its owner.
#[derive(Debug, Serialize)]
struct Hub {
    path: String,
    repo: Option<String>,
    kind: &'static str,
    refs_structural: usize,
    refs_navigational: usize,
    refs_total: usize,
}

/// The `graph_shape` read result: the scalar whole-graph summary. Raw facts, no
/// thresholds — the paper's numbers come from a 22-node corpus and are not
/// evidence, so the consumer applies its own policy.
#[derive(Debug, Serialize)]
struct GraphShapeView {
    /// The RESOLVED `repo` / `scope` this map was computed at, echoed back, so a
    /// consumer holds the scope it drilled at. Same rationale as `overview`.
    repo: Option<String>,
    scope: &'static str,
    /// Files in scope (the catalog node universe), and resolved edges in scope
    /// (both endpoints in scope). `edge_count` splits by the `slot.is_some()`
    /// partition `hubs` uses, so density per basis is derivable.
    node_count: usize,
    edge_count: usize,
    edges_structural: usize,
    edges_navigational: usize,
    /// Weakly-connected-component count over the undirected combined graph, the
    /// headline island count, and the node count of the biggest component (which
    /// distinguishes "14 tiny islands" from "one web plus 13 strays").
    components: usize,
    largest_component: usize,
    orphans: OrphansView,
    degree: DegreeView,
    /// `edge_count / node_count`, a convenience over the raw counts above.
    density: f64,
}

/// The two orphan definitions. `no_inbound` (nothing points at it) is the useful
/// one; `isolated` (no inbound AND no outbound, a singleton component) is the
/// strict subset. `paths` is present only when the read's `orphan_paths` is set.
#[derive(Debug, Serialize)]
struct OrphansView {
    no_inbound: usize,
    isolated: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    paths: Option<Vec<OrphanPath>>,
}

/// One orphan file, tagged so `no_inbound`-but-not-`isolated` is distinguishable
/// from `isolated` without a second pass.
#[derive(Debug, Serialize)]
struct OrphanPath {
    path: String,
    kind: &'static str,
}

/// The in- and out-degree distributions, each a histogram. The distribution
/// SHAPE is the signal, a few high-degree hubs over a long low-degree tail
/// versus a flat spread; `hubs` already serves the per-node top of it.
#[derive(Debug, Serialize)]
struct DegreeView {
    #[serde(rename = "in")]
    inbound: Vec<DegreeBucket>,
    out: Vec<DegreeBucket>,
}

/// `count` nodes carry exactly `degree` edges on this axis. Sorted by `degree`.
#[derive(Debug, Serialize, PartialEq)]
struct DegreeBucket {
    degree: usize,
    count: usize,
}

/// The `link_graph` read result: the full node+edge payload a whole-graph
/// visualization lays out. Nodes and edges are sorted for a deterministic
/// payload (and a stable diff for the `link_graph` subscription).
#[derive(Debug, Serialize)]
struct LinkGraphView {
    /// The RESOLVED `repo` / `scope`, echoed back, as `graph_shape` and
    /// `overview` do.
    repo: Option<String>,
    scope: &'static str,
    nodes: Vec<GraphNodeView>,
    edges: Vec<GraphEdgeView>,
}

/// A graph node: one content file. Carries the same hub counts `hubs` computes,
/// so the layout sizes a node for free. `refs_navigational` is
/// `refs_total - refs_structural`. Counts are over the IN-SCOPE inbound edges
/// (the induced subgraph), so they match the edge list in the same payload.
/// Isolated files (no edges) are present — a graph view must draw floating
/// orphans.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct GraphNodeView {
    path: String,
    repo: Option<String>,
    kind: &'static str,
    refs_structural: usize,
    refs_total: usize,
}

/// A resolved reference edge, source to target. `kind` is the coarse edge kind
/// (`field` / `contributing` / `navigational`), `surface` the referrer surface
/// (`frontmatter` / `body`), the same vocabulary `references_in` serves. Only
/// resolved edges exist here; a dangling link is a diagnostic, not an edge.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct GraphEdgeView {
    from: String,
    to: String,
    kind: &'static str,
    surface: &'static str,
}

/// A shutdown rendezvous: a `watch` channel flipped to `true` once a shutdown
/// request arrives. Connection tasks hold senders; the foreground waiter holds
/// a receiver. `watch` retains the latest value, so a shutdown that lands
/// before the waiter parks is not lost.
type ShutdownTx = watch::Sender<bool>;

/// A running server. Owns the tokio runtime driving the accept loop and every
/// connection task. Dropping it removes the socket file and shuts the runtime
/// down; the accept loop ends when the listener is dropped with it.
pub struct ServeHandle {
    socket: PathBuf,
    /// `Option` so `Drop` can `take` it and shut it down in the background.
    runtime: Option<Runtime>,
    /// Signals the accept loop and connection tasks to stop. Held so the watch
    /// channel stays open even if the accept loop ends, and flipped on `Drop`.
    shutdown_tx: ShutdownTx,
    shutdown_rx: watch::Receiver<bool>,
}

impl Drop for ServeHandle {
    fn drop(&mut self) {
        // Tell the accept loop and live connections to stop, then shut the
        // runtime down WITHOUT blocking this thread. Dropping a multi-thread
        // `Runtime` directly blocks on in-flight `spawn_blocking` tasks, which
        // is the teardown hang under the parallel test harness;
        // `shutdown_background` never blocks.
        let _ = self.shutdown_tx.send(true);
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl ServeHandle {
    /// Block until a `Shutdown` request arrives over the socket.
    ///
    /// The daemon's foreground loop parks here; `au daemon stop` connecting and
    /// sending the shutdown verb wakes it. Returns once asked to stop, or once
    /// the channel closes; dropping the handle then removes the socket.
    pub fn wait_for_shutdown(&self) {
        let mut rx = self.shutdown_rx.clone();
        if let Some(runtime) = self.runtime.as_ref() {
            runtime.block_on(async move {
                let _ = rx.wait_for(|&v| v).await;
            });
        }
    }
}

/// Bind a Unix socket at `socket` and serve reads against `handle` until asked
/// to shut down. A private tokio runtime drives the accept loop and one task
/// per connection.
///
/// The socket is created `0o600`: the wire is unauthenticated, so the trust
/// boundary is the owning user. This is a single-user surface; a multi-user
/// deployment would need real caller authentication, not just file mode.
pub fn serve(handle: EngineHandle, socket: impl Into<PathBuf>) -> std::io::Result<ServeHandle> {
    let socket = socket.into();

    // A daemon serves one local socket. Its async layer only multiplexes I/O,
    // the heavy fs / git / validation work runs on `spawn_blocking`. So a small
    // fixed worker count is right, and the default one-worker-per-core would
    // only explode the thread count (badly so under the test harness, which
    // runs many in-process daemons on parallel threads). `max_blocking_threads`
    // is the scaling knob for throughput under many repos or many users.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(512)
        .enable_all()
        .build()?;
    // The bind is the mutual-exclusion primitive: the OS lets exactly one
    // process bind a given path, so a live daemon's socket is never clobbered.
    // A pre-existing path fails with `AddrInUse`. Distinguish a live daemon
    // (connect succeeds) from a stale socket left by an ungraceful exit
    // (connect refused); only a confirmed-stale socket is removed and the bind
    // retried. The earlier code removed the path unconditionally, which would
    // unlink a live peer's socket on any probe false-negative.
    //
    // Residual: two concurrent starts both finding the SAME stale socket can
    // still race the reclaim. Closing that window fully needs an OS file lock
    // held for the daemon's lifetime (a dependency or MSRV 1.89's
    // `File::try_lock`); it is out of scope for the single-user surface.
    let bind = |rt: &Runtime| rt.block_on(async { tokio::net::UnixListener::bind(&socket) });
    let listener = match bind(&runtime) {
        Ok(l) => l,
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            if UnixStream::connect(&socket).is_ok() {
                // A live daemon owns the socket; leave it be.
                return Err(e);
            }
            // Stale socket from an ungraceful exit: reclaim it.
            std::fs::remove_file(&socket)?;
            bind(&runtime)?
        }
        Err(e) => return Err(e),
    };
    // Restrict the socket to the owning user: the wire is unauthenticated.
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let accept_tx = shutdown_tx.clone();
    runtime.spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let handle = handle.clone();
            let shutdown_tx = accept_tx.clone();
            tokio::spawn(async move {
                let _ = serve_connection(handle, stream, shutdown_tx).await;
            });
        }
    });
    // The accept loop is not signalled directly; `ServeHandle::Drop` shuts the
    // runtime down in the background, which cancels this task and every
    // connection task. That is what stops a `wait_for_shutdown` daemon once the
    // handle drops, and what makes test teardown non-blocking.

    Ok(ServeHandle {
        socket,
        runtime: Some(runtime),
        shutdown_tx,
        shutdown_rx,
    })
}

/// Serve one connection: a read loop, a writer task, and a session of
/// per-subscription tasks.
///
/// The stream is split. A writer task owns the write half and drains the frame
/// channel, so a response and a change event never interleave a partial frame.
/// The read loop owns the read half: it answers reads inline, and a `subscribe`
/// spawns a subscription task into the session. When the read loop ends, the
/// session is aborted and the frame channel closes, ending the writer.
async fn serve_connection(
    handle: EngineHandle,
    stream: tokio::net::UnixStream,
    shutdown_tx: ShutdownTx,
) -> std::io::Result<()> {
    let (mut read_half, mut write_half) = stream.into_split();

    let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(64);
    let writer = tokio::spawn(async move {
        while let Some(bytes) = frame_rx.recv().await {
            if write_frame_async(&mut write_half, &bytes).await.is_err() {
                break;
            }
        }
    });

    // The session: one task per active subscription, all aborted when the
    // connection ends. Each is assigned a per-connection id in its ack.
    let mut session: JoinSet<()> = JoinSet::new();
    let mut next_subscription_id: u64 = 0;

    while let Some(frame) = read_frame_async(&mut read_half).await? {
        let mut value: serde_json::Value = match serde_json::from_slice(&frame) {
            Ok(v) => v,
            Err(e) => {
                // The bytes are not JSON, so there is no id to echo.
                send_frame(
                    &frame_tx,
                    error_frame(format!("malformed request: {e}"), None, "unknown"),
                )
                .await;
                continue;
            }
        };

        // The optional client-supplied correlation id, echoed on the reply so
        // the client settles by id rather than by arrival order. Removed before
        // the verb payload is deserialized: the arg structs deny unknown fields,
        // so a lingering `id` sibling would be rejected as a bad argument.
        let id = value.as_object_mut().and_then(|obj| obj.remove("id"));

        if value.get("read").is_some() {
            // The verb string for the trace span, captured before `value` is
            // consumed. A no-op when tracing is off.
            let verb = value
                .get("read")
                .and_then(|v| v.as_str())
                .unwrap_or("read")
                .to_string();
            match serde_json::from_value::<Request>(value) {
                Ok(req) => {
                    let response = handle_request(&handle, req, &shutdown_tx)
                        .instrument(tracing::info_span!("read", verb = %verb))
                        .await
                        .with_id(id);
                    let bytes = serde_json::to_vec(&response).expect("response serializes");
                    if frame_tx.send(bytes).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    send_frame(
                        &frame_tx,
                        error_frame(format!("malformed read: {e}"), id, "read"),
                    )
                    .await;
                }
            }
        } else if value.get("subscribe").is_some() {
            match serde_json::from_value::<Subscription>(value) {
                Ok(sub) => {
                    let sub_id = next_subscription_id;
                    next_subscription_id += 1;
                    let handle = handle.clone();
                    let frame_tx = frame_tx.clone();
                    session.spawn(async move {
                        run_subscription(handle, frame_tx, sub_id, id, sub).await;
                    });
                }
                Err(e) => {
                    send_frame(
                        &frame_tx,
                        error_frame(format!("malformed subscribe: {e}"), id, "subscribe"),
                    )
                    .await;
                }
            }
        } else if value.get("mutate").is_some() {
            let verb = value
                .get("mutate")
                .and_then(|v| v.as_str())
                .unwrap_or("mutate")
                .to_string();
            match serde_json::from_value::<Mutation>(value) {
                Ok(m) => {
                    let mut frame = handle_mutation(&handle, m)
                        .instrument(tracing::info_span!("mutation", verb = %verb))
                        .await;
                    stamp_frame_id(&mut frame, &id);
                    if !send_frame(&frame_tx, frame).await {
                        break;
                    }
                }
                Err(e) => {
                    send_frame(
                        &frame_tx,
                        error_frame(format!("malformed mutate: {e}"), id, "mutate"),
                    )
                    .await;
                }
            }
        } else if value.get("resolve").is_some() {
            let mut frame = handle_resolve(&handle).await;
            stamp_frame_id(&mut frame, &id);
            if !send_frame(&frame_tx, frame).await {
                break;
            }
        } else {
            send_frame(
                &frame_tx,
                error_frame(
                    "request names neither a read, a subscribe, a mutate, nor a resolve"
                        .to_string(),
                    id,
                    "unknown",
                ),
            )
            .await;
        }
    }

    // Connection closed: abort the subscriptions, drop the frame sender so the
    // writer drains and ends.
    session.shutdown().await;
    drop(frame_tx);
    let _ = writer.await;
    Ok(())
}

/// Serialize a frame and hand it to the writer. Returns false when the channel
/// is closed (the connection went away), the producer task's cue to end.
async fn send_frame(frame_tx: &FrameTx, frame: serde_json::Value) -> bool {
    let bytes = serde_json::to_vec(&frame).expect("frame serializes");
    frame_tx.send(bytes).await.is_ok()
}

/// Stamp a request's `id` onto a value-built reply frame (mutation responses),
/// so the client settles it by id. A no-op when the request carried none.
fn stamp_frame_id(frame: &mut serde_json::Value, id: &Option<serde_json::Value>) {
    if let (Some(id), Some(obj)) = (id, frame.as_object_mut()) {
        obj.insert("id".to_string(), id.clone());
    }
}

/// An `error` frame: a request frame the daemon could not handle. `for_kind`
/// names the request shape (`read` | `subscribe` | `mutate` | `resolve` |
/// `unknown`) so a client can route the error even without an id; `id` echoes
/// the request's correlation id when it carried one.
fn error_frame(
    message: String,
    id: Option<serde_json::Value>,
    for_kind: &str,
) -> serde_json::Value {
    let mut frame = serde_json::json!({
        "type": "error",
        "schema_version": SCHEMA_VERSION,
        "error": message,
        "for": for_kind,
    });
    stamp_frame_id(&mut frame, &id);
    frame
}

/// An `ack` frame: the immediate reply to a `subscribe`. Echoes the request's
/// `id`, so the client ties this subscription to its request before the
/// per-subscription frames (which carry `subscription_id`) arrive.
fn ack_frame(
    subscription_id: u64,
    channel: &str,
    request_id: &Option<serde_json::Value>,
) -> serde_json::Value {
    let mut frame = serde_json::json!({
        "type": "ack",
        "schema_version": SCHEMA_VERSION,
        "subscription_id": subscription_id,
        "channel": channel,
        "accepted": true,
    });
    stamp_frame_id(&mut frame, request_id);
    frame
}

/// An `initial_value` frame: a channel's current state, delivered once after
/// the ack.
fn initial_value_frame(
    subscription_id: u64,
    at_version: u64,
    result: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "type": "initial_value",
        "schema_version": SCHEMA_VERSION,
        "subscription_id": subscription_id,
        "at_version": at_version,
        "result": result,
    })
}

/// A `change_event` frame with the coarse whole-knowledge-base scope hint. Only the
/// `ready` channel still carries it; the data channels compute precise hints.
fn change_event_frame(subscription_id: u64, kind: &str, at_version: u64) -> serde_json::Value {
    serde_json::json!({
        "type": "change_event",
        "schema_version": SCHEMA_VERSION,
        "subscription_id": subscription_id,
        "kind": kind,
        "at_version": at_version,
        "scope_hint": { "scope": "knowledge-base" },
    })
}

/// The standard reject for a write whose owning member is consumed (`discover` /
/// `dep`): the engine may not author it. Shared by every write surface that
/// resolves an owning member — the path-arg gate below, `rename_type`'s
/// graph-derived def, and the config channel — so the check is one expression, not
/// a per-surface reinvention.
fn not_editable_reject(
    name: &crate::repo::RepoName,
    role: crate::repo::MemberRole,
) -> MutationReject {
    MutationReject::new(format!(
        "member '{}' is mounted as '{}', not editable — list it in workspace.yaml 'edit:' to author it",
        name.as_str(),
        role.as_str(),
    ))
}

/// Resolve a mutation path against the held knowledge base's members, then gate it
/// on the owning member's EDITABILITY. A write reaches any declared member the same
/// way a read does, but only an EDITABLE member (the entry or an `edit` member) may
/// be authored through the channel; a consumed member (`discover` / `dep`) is
/// rejected. This gate is what keeps a write off an unwatched, regenerable cache
/// snapshot (a silent-wrong-result), the write path's half of the read-side
/// role-derived editability.
///
/// Both mutation surfaces route through here, so preview reports the identical reject
/// the real write would.
fn resolve_editable_target(
    kb: &KnowledgeBase,
    root: &Path,
    path: &str,
) -> Result<std::path::PathBuf, MutationReject> {
    let target = crate::mutate::resolve_repo_path(&kb.repos, root, path)?;
    // The owning member is guaranteed by `resolve_repo_path` (a path under no member
    // is already the escape reject), so `repo_of` is `Some` here.
    let member = kb
        .repos
        .repo_of(&target)
        .expect("resolve_repo_path guarantees an owning member");
    let role = crate::wire::member_role(kb, &member.name);
    if !role.editable() {
        return Err(not_editable_reject(&member.name, role));
    }
    Ok(target)
}

/// Resolve a mutation path against the held knowledge base's members. A write reaches any
/// declared member, scattered or subdir, the same way a read does. `Err` carries a
/// ready frame the caller returns as-is: a reject for an escaping/guarded/non-editable
/// path, or not-ready if the ref is mid-build.
fn resolve_mutation_target(
    handle: &EngineHandle,
    path: &str,
) -> Result<std::path::PathBuf, serde_json::Value> {
    let root = handle.root().to_path_buf();
    match handle
        .read(move |v| resolve_editable_target(v, &root, path))
        .value()
    {
        Some(Ok(target)) => Ok(target),
        Some(Err(reject)) => Err(mutation_reject_frame(reject)),
        None => Err(serde_json::to_value(Response::not_ready()).expect("response serializes")),
    }
}

/// One member of a mutation's saga: a git working tree the mutation touches,
/// with the tree-relative paths it writes there.
///
/// The member is the TREE, not the repo. Git commits per working tree and one
/// mutation makes one commit per tree, so several daemon-owned repos inside one
/// tree coalesce into a single member holding the union of their paths.
struct SagaMember {
    /// The working tree this member's paths commit into. For a non-git member
    /// (no enclosing tree anywhere) it is the owning repo root, which never
    /// commits.
    tree_root: PathBuf,
    /// The daemon-owned repos whose touched paths this tree holds, name-sorted.
    /// Several for a monorepo, one for a repo that is its own working tree.
    repo_names: Vec<crate::repo::RepoName>,
    /// Paths relative to `tree_root`, the basis the commit, the clean check, and
    /// the path-restore all take.
    paths: Vec<PathBuf>,
    /// Whether the member has a git working tree. A non-git member is written but
    /// never committed, the transition fallback until it is materialized under
    /// git.
    git: bool,
}

/// A mutation's per-repo plan, applied as one local saga.
///
/// The member set is the daemon-owned repos the mutation touches, each resolved
/// from a touched path by `repo_of` (the deepest owning repo). Every commit of
/// the saga carries the shared `mutation_id`. Single-repo is the N=1 case of the
/// same structure, not a separate path.
struct SagaPlan {
    mutation_id: String,
    members: Vec<SagaMember>,
    /// Absolute paths in the member set that the mutation will NOT write, so the
    /// clean-at-HEAD guard must not check them.
    ///
    /// A referrer whose every reference is commit-pinned is rewritten to
    /// byte-identical content, so nothing is written to it. Gating on it lets a
    /// file the refactor would not touch reject the whole refactor. It stays in
    /// the member set regardless, because it still needs RECOMPUTING: a frozen
    /// pin stops resolving live once its target moves.
    ///
    /// Empty for every mutation that writes everything it touches.
    exempt_from_clean_check: std::collections::BTreeSet<PathBuf>,
    /// A caller-supplied opaque attribution payload, folded into every commit
    /// this saga makes as trailers beside `Mutation-Id`. Validated against the
    /// reserved keys at the handler boundary. Empty for a mutation carrying no
    /// attribution (and for every refactor today). See
    /// [[spec - git write path - commit-per-mutation as a local saga over the workspace's materialized repos]].
    attribution: Vec<crate::gitwriter::CommitTrailer>,
}

/// Resolve a mutation's member set from the absolute paths it touches.
///
/// Two stages, answering two different questions.
/// - SCOPE, which paths are in play: each path's owning repo (`repo_of`, the
///   deepest owning repo). A path with no owning repo cannot occur for a
///   resolved mutation target and is dropped.
/// - GROUPING, how those paths become members: each repo's enclosing git working
///   tree. Several repos in one tree COALESCE into one member holding the union
///   of their paths, so one mutation makes one commit per tree.
///
/// A repo with no enclosing tree keys on its own root and stays non-git: written,
/// never committed.
///
/// Members are tree-root-ordered and every name list is sorted, so the commit
/// trailer's member list is deterministic.
fn resolve_saga_plan(handle: &EngineHandle, targets: &[PathBuf]) -> SagaPlan {
    // Stage one: absolute touched paths per owning repo.
    let by_repo = handle
        .read(|v| {
            let mut by_repo: BTreeMap<PathBuf, (crate::repo::RepoName, Vec<PathBuf>)> =
                BTreeMap::new();
            for t in targets {
                if let Some(repo) = v.repos.repo_of(t) {
                    by_repo
                        .entry(repo.root.clone())
                        .or_insert_with(|| (repo.name.clone(), Vec::new()))
                        .1
                        .push(t.clone());
                }
            }
            by_repo
        })
        .value()
        .unwrap_or_default();

    SagaPlan {
        mutation_id: crate::mutate::generate_mutation_id(),
        members: coalesce_by_working_tree(by_repo),
        exempt_from_clean_check: std::collections::BTreeSet::new(),
        // Set by a handler that carries a caller attribution payload.
        attribution: Vec::new(),
    }
}

/// The referrers a refactor will NOT write: those whose every reference is
/// commit-pinned, so the rewrite is byte-identical.
///
/// Answers from [`crate::rename::would_rewrite`], the same predicate `ref_edits`
/// skips on, so the planner and the rewriter cannot disagree about which files
/// the mutation writes.
///
/// A referrer whose content cannot be read here is treated as WRITABLE, so it
/// stays gated. The failure direction matters: guessing "writable" costs a
/// conservative rejection, guessing "exempt" would let the mutation write over
/// uncommitted work unchecked.
fn referrers_not_written(
    referrers: &BTreeMap<PathBuf, Vec<au_diagnostics::ByteRange>>,
) -> std::collections::BTreeSet<PathBuf> {
    referrers
        .iter()
        .filter(|(path, spans)| match std::fs::read_to_string(path) {
            Ok(content) => !crate::rename::would_rewrite(&content, spans),
            Err(_) => false,
        })
        .map(|(path, _)| path.clone())
        .collect()
}

/// The referrer paths a refactor WILL rewrite: its whole referrer set minus the
/// commit-pinned, byte-identical ones. The write-set the editability gate below
/// classifies, so a pinned referrer in a consumed member never blocks.
fn written_referrers(
    referrers: &BTreeMap<PathBuf, Vec<au_diagnostics::ByteRange>>,
    not_written: &std::collections::BTreeSet<PathBuf>,
) -> std::collections::BTreeSet<PathBuf> {
    referrers
        .keys()
        .filter(|p| !not_written.contains(*p))
        .cloned()
        .collect()
}

/// The written referrers that live in a consumed (non-editable) member, grouped
/// by owning member. A refactor rewrites referrers to keep the mounted-set graph
/// consistent, but it may not author a member it does not own, so any such
/// referrer blocks the whole refactor.
fn consumed_written_referrers(
    v: &KnowledgeBase,
    written: &std::collections::BTreeSet<PathBuf>,
) -> BTreeMap<crate::repo::RepoName, Vec<PathBuf>> {
    let mut blocking: BTreeMap<crate::repo::RepoName, Vec<PathBuf>> = BTreeMap::new();
    for path in written {
        if let Some(repo) = v.repos.repo_of(path) {
            if !crate::wire::member_role(v, &repo.name).editable() {
                blocking
                    .entry(repo.name.clone())
                    .or_default()
                    .push(path.clone());
            }
        }
    }
    for paths in blocking.values_mut() {
        paths.sort();
    }
    blocking
}

/// Reject a refactor when any referrer it would rewrite lives in a consumed
/// member. The `detail` carries the blocking `(member -> files)` list so a
/// consumer renders the choice: promote the member to `edit`, or (the tracked
/// future opt-in) skip it and leave an advisory dangling reference.
fn reject_if_consumed_referrers(
    handle: &EngineHandle,
    written: &std::collections::BTreeSet<PathBuf>,
) -> Option<crate::mutate::MutationReject> {
    let blocking = handle
        .read(|v| consumed_written_referrers(v, written))
        .value()
        .unwrap_or_default();
    if blocking.is_empty() {
        return None;
    }
    let members: Vec<String> = blocking.keys().map(|n| n.as_str().to_string()).collect();
    let mut detail = serde_json::Map::new();
    for (member, paths) in &blocking {
        detail.insert(
            member.as_str().to_string(),
            serde_json::Value::from(
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>(),
            ),
        );
    }
    Some(crate::mutate::MutationReject {
        message: format!(
            "cannot rewrite references in consumed member(s) {} — a refactor may not author a \
             member it does not own; list them in workspace.yaml 'edit:' to rewrite their references",
            members.join(", "),
        ),
        detail: Some(serde_json::json!({ "blocking_consumed_referrers": detail })),
    })
}

/// Group per-repo absolute paths into saga members, one per git working tree.
///
/// The grouping half of [`resolve_saga_plan`], split out because it is the whole
/// correctness claim and is testable without an engine: several repos inside one
/// tree must come back as ONE member, or a mutation lands N commits sharing a
/// `Mutation-Id` and compensation can only reach the newest.
fn coalesce_by_working_tree(
    by_repo: BTreeMap<PathBuf, (crate::repo::RepoName, Vec<PathBuf>)>,
) -> Vec<SagaMember> {
    type Acc = (
        std::collections::BTreeSet<crate::repo::RepoName>,
        Vec<PathBuf>,
        bool,
    );
    let mut by_tree: BTreeMap<PathBuf, Acc> = BTreeMap::new();
    for (repo_root, (name, abs_paths)) in by_repo {
        let (root, git) = match crate::gitwriter::working_tree_of(&repo_root) {
            Some(tree) => (tree, true),
            None => (repo_root, false),
        };
        let entry = by_tree
            .entry(root.clone())
            .or_insert_with(|| (std::collections::BTreeSet::new(), Vec::new(), git));
        entry.0.insert(name);
        for abs in abs_paths {
            // Rebase onto the tree, the basis the commit takes. `repo_of` put the
            // path inside the repo and the tree encloses the repo, so this holds.
            if let Ok(rel) = abs.strip_prefix(&root) {
                entry.1.push(rel.to_path_buf());
            }
        }
    }
    by_tree
        .into_iter()
        .map(|(tree_root, (names, paths, git))| SagaMember {
            tree_root,
            repo_names: names.into_iter().collect(),
            paths,
            git,
        })
        .collect()
}

/// The absolute paths a saga touches across every member, the dirty set the
/// single post-settle rebuild reads. Spans all members, git and non-git alike: a
/// non-git member's written file is still re-read into the IR. One rebuild over
/// this whole set bumps the version once, so a multi-repo mutation is observed as
/// one change after the saga settles, never per-repo mid-saga.
fn saga_dirty_set(plan: &SagaPlan) -> std::collections::BTreeSet<PathBuf> {
    plan.members
        .iter()
        .flat_map(|m| m.paths.iter().map(|p| m.tree_root.join(p)))
        .collect()
}

/// The type-def named `name`, the rename target.
fn find_owned_type_def<'a>(
    v: &'a crate::ir::KnowledgeBase,
    name: &au_core::TypeName,
    in_repo: Option<&str>,
) -> Option<&'a au_core::TypeDef> {
    for repo in v.repos.repos() {
        // A `::repo`-qualified target selects ONE owner's identity; without it,
        // the first owner across the mounted set wins (the historical behaviour).
        if in_repo.is_some_and(|want| repo.name.as_str() != want) {
            continue;
        }
        if let Some(td) = v.graphs.of(&repo.name).get(name) {
            return Some(td);
        }
    }
    None
}

/// The path a type-def file takes when its type is renamed to `new_name`. The
/// type-name derives from the filename's stem before its known suffix, so the
/// rename swaps that stem and keeps the suffix and directory. The inverse of
/// [`au_core::type_name_from_path`].
fn renamed_type_def_path(from: &Path, new_name: &au_core::TypeName) -> Option<PathBuf> {
    let file_name = from.file_name()?.to_str()?;
    let suffix = [".type.yaml", ".type.yml", ".yaml", ".yml"]
        .into_iter()
        .find(|s| file_name.ends_with(s))?;
    Some(from.with_file_name(format!("{}{suffix}", new_name.as_str())))
}

/// Everything `rename_type` gathers under the read lock, before the saga writes.
/// Carries the def's file relocation and every referrer to rewrite.
struct RenameTypeGather {
    /// The def's current path.
    from: PathBuf,
    /// The def's new path.
    to: PathBuf,
    /// The repo root the def's file move belongs to, for the `Moved:` record.
    move_repo_root: PathBuf,
    /// The def's repo-relative paths, for the wikilink-surface rewrite.
    old_rel: PathBuf,
    new_rel: PathBuf,
    /// Type-name-reference edits per referrer (surface 1).
    type_edits: BTreeMap<PathBuf, Vec<(au_diagnostics::ByteRange, String)>>,
    /// Wikilink-to-def spans per referrer (surface 2).
    link_spans: BTreeMap<PathBuf, Vec<au_diagnostics::ByteRange>>,
    /// Each referrer's held content hash, the write-time re-read guard.
    referrer_hashes: BTreeMap<PathBuf, Option<crate::ir::ContentHash>>,
    /// Every referrer path to rewrite, deduped across both surfaces.
    referrers: Vec<PathBuf>,
}

/// The moves a saga's commits record, keyed by repo root, so each member's
/// commit carries only its own `Moved:` trailers. A rename's referrers can span
/// repos, so the move belongs only to the repo where the file actually moved.
/// Empty for a non-move mutation.
type MovesByTree = std::collections::BTreeMap<PathBuf, Vec<crate::gitwriter::MovedRecord>>;

/// The `Moved:` record for a file move, keyed for the commit that will carry it.
///
/// Keyed by the WORKING TREE holding the repo, with the paths rebased onto that
/// tree. That is the commit's own basis: `settle_saga` looks these up by the
/// member's `tree_root`, and a repo nested in a larger tree commits into the
/// tree. Keyed by the repo root instead, a nested repo's rename would find no
/// entry and silently carry no trailer, breaking the forward trace without
/// failing anything.
///
/// The trace reader (`moved_trailers_since`) is not yet surfaced, so this basis
/// is established before it has a consumer rather than migrated after.
fn moves_for(repo_root: &Path, old_rel: &Path, new_rel: &Path) -> MovesByTree {
    let tree =
        crate::gitwriter::working_tree_of(repo_root).unwrap_or_else(|| repo_root.to_path_buf());
    let onto_tree = |rel: &Path| {
        repo_root
            .join(rel)
            .strip_prefix(&tree)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| rel.to_path_buf())
    };
    let record = crate::gitwriter::MovedRecord {
        from: onto_tree(old_rel),
        to: onto_tree(new_rel),
    };
    let mut moves = MovesByTree::new();
    moves.insert(tree, vec![record]);
    moves
}

/// The commits a saga made, one per committing member, each paired with the repo
/// that committed it. Name-sorted by member. Empty when nothing committed (every
/// member off-git, or a no-op that wrote nothing). The mutate result projects
/// this as `commits: { [repo]: sha }`, and derives the `commit` scalar from the
/// response file's own repo.
type SagaCommits = Vec<(crate::repo::RepoName, CommitSha)>;

/// `apply_saga` for a mutation that records no moves, the common case.
///
/// `require_git` rejects the whole saga up front if any member is non-git: a
/// structural refactor rewrites references across its members and compensates
/// git-only, so a non-git member could not be rolled back. A simple single-file
/// write passes `false`, its write-without-commit fallback is intended.
fn apply_saga<T>(
    git: &impl GitWriter,
    marker_dir: &Path,
    plan: &SagaPlan,
    summary: String,
    require_git: bool,
    write: impl FnOnce() -> Result<T, MutationReject>,
) -> Result<(T, SagaCommits), MutationReject> {
    apply_saga_with_moves(
        git,
        marker_dir,
        plan,
        summary,
        &MovesByTree::new(),
        require_git,
        write,
    )
}

/// Apply a mutation as one local saga over its member set: the cross-repo
/// clean-at-HEAD precondition, the durable intent marker, the write, then
/// commit-each-correlated, with git as the compensation on failure. Returns the
/// write's value and the commits made, one per committing member. Single-repo is
/// the N=1 case, a non-git member writes without committing.
///
/// `git` is the swappable mechanism; production passes `&ShellGit`. The port is
/// injected so the saga's compensation logic is testable against a writer that
/// fails a chosen member's commit. `marker_dir` is [`saga_marker_dir`] (the launch
/// entry's `.arsumbris/au-engine/run/`), a single per-daemon location, where the intent marker opens and
/// closes the saga across all its members. `moves` records each member's renames
/// as `Moved:` trailers for the pinned-reference forward trace.
fn apply_saga_with_moves<T>(
    git: &impl GitWriter,
    marker_dir: &Path,
    plan: &SagaPlan,
    summary: String,
    moves: &MovesByTree,
    require_git: bool,
    write: impl FnOnce() -> Result<T, MutationReject>,
) -> Result<(T, SagaCommits), MutationReject> {
    // Require-git precondition: a structural refactor rewrites references across
    // its member set and compensates git-only (revert-by-checkout), so a member
    // written mid-saga with no working tree to revert could not be rolled back.
    // An all-or-nothing reject before the saga opens, never a half-apply.
    //
    // A member nested inside a larger working tree IS covered by it, so this
    // refuses only a member with no enclosing tree anywhere, not every repo
    // lacking a `.git` of its own.
    if require_git {
        let non_git: Vec<&str> = plan
            .members
            .iter()
            .filter(|m| !m.git)
            .flat_map(|m| m.repo_names.iter().map(|n| n.as_str()))
            .collect();
        if !non_git.is_empty() {
            return Err(MutationReject::new(format!(
                "refactor spans a member no git working tree covers ({}): compensation is git-only, so a mid-saga failure could not be rolled back. Refusing. Put it under a working tree, its own root or any ancestor",
                non_git.join(", ")
            )));
        }
    }

    // Only git working trees commit; a non-git member is written but not committed.
    let git_members: Vec<&SagaMember> = plan.members.iter().filter(|m| m.git).collect();
    // The complete member set every commit carries, so any one commit names all
    // committing repos. One member can cover several repos (a monorepo tree), so
    // this is the union across members.
    //
    // Sorted by NAME, not left in member order. Members are tree-ordered, which
    // is an implementation detail of the grouping and would put the trailer's
    // names in an order no reader can predict. This trailer is read by a human in
    // `git log` and parsed by crash recovery, so it is name-sorted for both.
    let mut member_names: Vec<crate::repo::RepoName> = git_members
        .iter()
        .flat_map(|m| m.repo_names.iter().cloned())
        .collect();
    member_names.sort();

    // Cross-repo clean precondition: every touched path in every member clean at
    // HEAD, an all-or-nothing early reject before the saga opens, before any
    // tree is touched.
    for m in &git_members {
        // Only the paths the mutation will actually write. A referrer the
        // refactor leaves byte-identical cannot have its work clobbered, so
        // gating on it would let a file the mutation never touches reject it.
        let checked: Vec<PathBuf> = m
            .paths
            .iter()
            .filter(|p| !plan.exempt_from_clean_check.contains(&m.tree_root.join(p)))
            .cloned()
            .collect();
        if checked.is_empty() {
            continue;
        }
        let clean = git
            .is_clean(&m.tree_root, &checked)
            .map_err(|e| MutationReject::new(format!("git precondition failed: {}", e.message)))?;
        if !clean {
            return Err(MutationReject::new(
                "path has uncommitted changes — the engine is the writer; commit or discard them first",
            ));
        }
    }

    // Open the saga: write the intent marker before any working tree is touched.
    let marker = write_intent_marker(marker_dir, plan).map_err(|e| {
        MutationReject::new(format!(
            "could not open the saga: intent marker write failed: {e}"
        ))
    })?;

    // The body settles to commit-all or compensate-all, then the marker closes.
    let outcome = settle_saga(
        git,
        &git_members,
        &member_names,
        plan,
        &summary,
        moves,
        marker_dir,
        write,
    );
    remove_intent_marker(&marker);
    outcome
}

/// The settling body of a saga: apply every write, commit each member correlated
/// by the shared `Mutation-Id`, and compensate every member on any failure, a
/// write failure or a commit failure alike. Always settles before returning, so
/// the caller can close the intent marker.
fn settle_saga<T>(
    git: &impl GitWriter,
    git_members: &[&SagaMember],
    member_names: &[crate::repo::RepoName],
    plan: &SagaPlan,
    summary: &str,
    moves: &MovesByTree,
    marker_dir: &Path,
    write: impl FnOnce() -> Result<T, MutationReject>,
) -> Result<(T, SagaCommits), MutationReject> {
    let mutation_id = MutationId(plan.mutation_id.clone());

    // Apply every write across all members' working trees. A partial write
    // compensates every member, then rejects.
    let value = match write() {
        Ok(v) => v,
        Err(reject) => {
            let errors = compensate(git, &[], git_members, &mutation_id);
            return Err(note_incomplete_rollback(
                marker_dir,
                &plan.mutation_id,
                errors,
                reject,
            ));
        }
    };

    // Commit each member, correlated by the shared Mutation-Id.
    let mut commits: SagaCommits = Vec::new();
    let mut committed: Vec<&SagaMember> = Vec::new();
    for m in git_members {
        // A no-op write leaves the member's paths clean at HEAD: no diff to
        // commit. Skip it rather than letting `git commit` fail on an empty diff,
        // the spec's "a no-op write makes no commit". An is_clean error is not a
        // clean signal, so fall through and let the commit decide.
        if git.is_clean(&m.tree_root, &m.paths).unwrap_or(false) {
            continue;
        }
        let message = CommitMessage {
            summary: summary.to_string(),
            mutation_id: mutation_id.clone(),
            members: member_names.to_vec(),
            moves: moves.get(&m.tree_root).cloned().unwrap_or_default(),
            reverts: None,
            attribution: plan.attribution.clone(),
        };
        match git.commit(&m.tree_root, &m.paths, &message) {
            Ok(sha) => {
                // The wire serves `commits: { [repo]: sha }`, so every repo this
                // tree covers names the one commit that carries its change. A
                // coalesced member yields several entries sharing a sha.
                for name in &m.repo_names {
                    commits.push((name.clone(), sha.clone()));
                }
                committed.push(*m);
            }
            Err(e) => {
                // Compensate: revert those already committed by Mutation-Id, then
                // restore every member's paths to HEAD. A compensation error is
                // recorded durably, never swallowed.
                let errors = compensate(git, &committed, git_members, &mutation_id);
                let reject = MutationReject::new(format!("commit failed: {}", e.message));
                return Err(note_incomplete_rollback(
                    marker_dir,
                    &plan.mutation_id,
                    errors,
                    reject,
                ));
            }
        }
    }
    Ok((value, commits))
}

/// The saga intent marker's file name under [`saga_marker_dir`]. The write
/// pipeline is serialized (one mutation at a time, see
/// `EngineHandle::write_lock`), so at most one saga is ever in flight and a
/// single fixed marker file is enough.
const INTENT_MARKER_NAME: &str = "saga.intent";

/// The engine's saga-recovery directory under the launch entry root,
/// `<root>/.arsumbris/au-engine/run/`. Holds the `saga.intent` marker and the
/// `saga.failed.<id>` sentinels: ephemeral run-state, owner-namespaced under the
/// engine, see
/// [[spec - arsumbris layout - a reserved multi-tenant device root, owner-namespaced with a category sublayer]].
fn saga_marker_dir(root: &Path) -> PathBuf {
    root.join(".arsumbris").join("au-engine").join("run")
}

/// Open a saga by writing its durable intent marker, fsynced. Returns the marker
/// path so the caller closes it on settle.
///
/// Format: a `mutation-id=<id>` line, then one line per committing member, the
/// member's absolute WORKING-TREE root, then its touched tree-relative paths,
/// all tab-separated. One line per tree, so a member covering several repos of a
/// monorepo records once, matching the one commit it makes. The per-member paths
/// are what crash recovery path-restores a written-but-not-committed member by;
/// the tree root makes recovery self-contained, no rebuilt IR needed. It records
/// intent, not the pending contents, so it can roll a crashed saga back but not
/// complete it.
///
/// Engine-internal, under `marker_dir` ([`saga_marker_dir`], the launch entry's
/// `.arsumbris/au-engine/run/`). That root is the entry repo's
/// directory, an ancestor of every co-present member but not of a scattered member
/// that sits elsewhere. Either way it is a single
/// per-daemon location, so one marker spans the whole cross-repo saga, never one
/// per member. It is known at launch, before the IR builds, which is what crash
/// recovery needs.
///
/// It closes the window the commit trailers cannot: a crash after writing the
/// trees but before the first commit leaves dirty trees and zero trailers,
/// indistinguishable from an unmediated edit without the marker. Crash recovery
/// reads it back to compensate an interrupted saga.
fn write_intent_marker(marker_dir: &Path, plan: &SagaPlan) -> std::io::Result<PathBuf> {
    use std::io::Write;
    std::fs::create_dir_all(marker_dir)?;
    let mut body = format!("mutation-id={}\n", plan.mutation_id);
    for m in plan.members.iter().filter(|m| m.git) {
        body.push_str(&m.tree_root.to_string_lossy());
        for p in &m.paths {
            body.push('\t');
            body.push_str(&p.to_string_lossy());
        }
        body.push('\n');
    }
    let path = marker_dir.join(INTENT_MARKER_NAME);
    let mut file = std::fs::File::create(&path)?;
    file.write_all(body.as_bytes())?;
    file.sync_all()?;
    // fsync the directory too, so the marker's existence survives a crash.
    if let Ok(dir) = std::fs::File::open(marker_dir) {
        let _ = dir.sync_all();
    }
    Ok(path)
}

/// A crashed saga rolled back at startup: the mutation it recovered, and which
/// member repos were reverted (they had committed) versus restored (they wrote
/// but did not commit).
#[derive(Debug)]
pub struct SagaRecovery {
    pub mutation_id: String,
    pub reverted: Vec<PathBuf>,
    pub restored: Vec<PathBuf>,
}

/// An un-rolled-back saga, surfaced from a `saga.failed.<id>` sentinel. It
/// persists until an operator resolves it, so it is re-surfaced at every start.
#[derive(Debug)]
pub struct SagaFailure {
    pub mutation_id: String,
    /// The sentinel's body: one line per member that could not be rolled back.
    pub detail: String,
}

/// What startup recovery found and did.
#[derive(Debug)]
pub struct SagaRecoveryReport {
    /// The intent marker that was processed, if one was open.
    pub recovered: Option<SagaRecovery>,
    /// Outstanding failure sentinels, this run's and any prior run's.
    pub failures: Vec<SagaFailure>,
}

/// Roll an interrupted saga back before serving, then report any outstanding
/// un-rolled-back sagas.
///
/// Reads the intent marker under [`saga_marker_dir`]. Each member it names is
/// compensated by the same rule the live saga uses: a member carrying the
/// mutation's commit is reverted by `Mutation-Id`, a member that wrote but did
/// not commit has its recorded paths restored to HEAD. The intent marker is then
/// removed. A member that cannot be rolled back (a revert conflict after a human
/// committed over the engine, or a classify error) is recorded in a durable
/// `saga.failed.<id>` sentinel, never silently dropped.
///
/// Recovery does not auto-retry a failure: a failed revert is most plausibly a
/// conflict needing human resolution, so the engine's job is to keep it visible,
/// not to loop. Every `saga.failed.<id>` sentinel is re-surfaced here at each
/// start until an operator removes it.
///
/// The intent marker is the only thing that distinguishes a crash-before-first-
/// commit (dirty trees, zero trailers) from an unmediated edit, so recovery never
/// guesses from the working tree alone.
pub fn recover_crashed_saga(root: &Path) -> SagaRecoveryReport {
    let marker_dir = saga_marker_dir(root);
    let recovered = recover_intent_marker(&marker_dir);
    let failures = scan_failure_sentinels(&marker_dir);
    SagaRecoveryReport {
        recovered,
        failures,
    }
}

/// Process the open intent marker, if any. Compensates each member, records any
/// member it could not roll back in a `saga.failed.<id>` sentinel, then removes
/// the intent marker.
fn recover_intent_marker(marker_dir: &Path) -> Option<SagaRecovery> {
    let marker_path = marker_dir.join(INTENT_MARKER_NAME);
    let content = std::fs::read_to_string(&marker_path).ok()?;
    let mut lines = content.lines();
    let mutation_id = lines.next()?.strip_prefix("mutation-id=")?.to_string();
    let id = MutationId(mutation_id.clone());

    let mut reverted = Vec::new();
    let mut restored = Vec::new();
    let mut errors: Vec<(PathBuf, String)> = Vec::new();
    for line in lines {
        let mut fields = line.split('\t');
        let Some(root_str) = fields.next().filter(|s| !s.is_empty()) else {
            continue;
        };
        let tree_root = PathBuf::from(root_str);
        let paths: Vec<PathBuf> = fields.map(PathBuf::from).collect();
        // One line per TREE, so one classification covers every repo that tree
        // holds: the coalesced member made one commit, and this reverts that one.
        match ShellGit.committed(&tree_root, &id) {
            Ok(true) => match ShellGit.revert_commit(&tree_root, &id) {
                Ok(()) => reverted.push(tree_root),
                Err(e) => errors.push((tree_root, format!("revert failed: {}", e.message))),
            },
            Ok(false) => match ShellGit.restore_paths(&tree_root, &paths) {
                Ok(()) => restored.push(tree_root),
                Err(e) => errors.push((tree_root, format!("restore failed: {}", e.message))),
            },
            Err(e) => errors.push((tree_root, format!("classify failed: {}", e.message))),
        }
    }
    if !errors.is_empty() {
        write_failure_sentinel(marker_dir, &mutation_id, &errors);
    }
    let _ = std::fs::remove_file(&marker_path);
    Some(SagaRecovery {
        mutation_id,
        reverted,
        restored,
    })
}

/// Read every `saga.failed.<id>` sentinel under the marker directory, name-sorted
/// for a deterministic report. They are not removed, an operator clears them
/// after manual repair.
fn scan_failure_sentinels(marker_dir: &Path) -> Vec<SagaFailure> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(marker_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(id) = name.strip_prefix("saga.failed.") {
            let detail = std::fs::read_to_string(entry.path()).unwrap_or_default();
            out.push(SagaFailure {
                mutation_id: id.to_string(),
                detail,
            });
        }
    }
    out.sort_by(|a, b| a.mutation_id.cmp(&b.mutation_id));
    out
}

/// Close a saga by removing its intent marker, on settle (commit-all or
/// compensate-all). Best-effort: a stale marker is detected and handled by crash
/// recovery, never a correctness hazard.
fn remove_intent_marker(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// The failure sentinel's file name, one per un-rolled-back saga, keyed by
/// `Mutation-Id`. Distinct from `saga.intent` and from another saga's failure,
/// so the next saga's marker never clobbers it.
fn failure_sentinel_name(mutation_id: &str) -> String {
    format!("saga.failed.{mutation_id}")
}

/// Record an incomplete rollback durably, so a dangling saga is never invisible.
/// One line per member that could not be rolled back, the repo root and why.
/// Surfaced at every startup, removed only by an operator after manual repair.
/// Best-effort: a sentinel write that fails cannot itself be compensated.
fn write_failure_sentinel(marker_dir: &Path, mutation_id: &str, errors: &[(PathBuf, String)]) {
    use std::io::Write;
    if std::fs::create_dir_all(marker_dir).is_err() {
        return;
    }
    let mut body = format!("mutation-id={mutation_id}\n");
    for (root, err) in errors {
        body.push_str(&format!("{}\t{}\n", root.to_string_lossy(), err));
    }
    let path = marker_dir.join(failure_sentinel_name(mutation_id));
    if let Ok(mut file) = std::fs::File::create(&path) {
        let _ = file.write_all(body.as_bytes());
        let _ = file.sync_all();
    }
}

/// On a saga whose rollback could not complete, write the failure sentinel and
/// tag the reject so the consumer learns the rollback was incomplete. No errors
/// is the clean case, the reject passes through unchanged.
fn note_incomplete_rollback(
    marker_dir: &Path,
    mutation_id: &str,
    errors: Vec<(PathBuf, String)>,
    mut reject: MutationReject,
) -> MutationReject {
    if errors.is_empty() {
        return reject;
    }
    write_failure_sentinel(marker_dir, mutation_id, &errors);
    reject.message = format!(
        "{}; the rollback was incomplete, {} member(s) recorded in {}",
        reject.message,
        errors.len(),
        failure_sentinel_name(mutation_id)
    );
    reject
}

/// Roll a failed saga back, returning the per-member errors it could not
/// compensate. Each already-committed member is reverted by its `Mutation-Id`
/// (idempotent, race-free against a concurrent human commit), then every member's
/// touched paths are restored to HEAD. Restoring a committed-then-reverted member
/// is a no-op, so passing all members is safe and removes the need to track which
/// were written-with-changes (a no-op write skips its commit but stays in the
/// member list).
///
/// A compensation error cannot itself be compensated, so the errors are returned
/// for the caller to record durably, never swallowed — a member that could not
/// be rolled back must not become invisible.
fn compensate(
    git: &impl GitWriter,
    committed: &[&SagaMember],
    members: &[&SagaMember],
    mutation_id: &MutationId,
) -> Vec<(PathBuf, String)> {
    let mut errors = Vec::new();
    for m in committed {
        if let Err(e) = git.revert_commit(&m.tree_root, mutation_id) {
            errors.push((m.tree_root.clone(), format!("revert failed: {}", e.message)));
        }
    }
    for m in members {
        if let Err(e) = git.restore_paths(&m.tree_root, &m.paths) {
            errors.push((
                m.tree_root.clone(),
                format!("restore failed: {}", e.message),
            ));
        }
    }
    errors
}

/// Fold a list of write stamps into a just-written file, in order, returning the
/// hash the response should carry: the stamped file's new hash when the last
/// non-no-op stamp changed it, else the primary write's hash. Each stamp re-reads
/// the just-written bytes, so a same-field append sees the growing list. Runs
/// INSIDE the write closure, so every stamp shares the write's commit
/// ([[spec - stamp injection - a write rider idempotently ensures a frontmatter record folded into the write's commit]]).
fn fold_write_stamps(
    target: &Path,
    stamps: &[Stamp],
    primary: crate::ir::ContentHash,
) -> Result<crate::ir::ContentHash, crate::mutate::MutationReject> {
    let mut hash = primary;
    for s in stamps {
        if let Some(h) =
            crate::mutate::fold_stamp(target, &s.field, &s.record, s.match_on.as_ref())?
        {
            hash = h;
        }
    }
    Ok(hash)
}

/// The serde default for `ensure_mixins_strict`: strict is the default, so an
/// un-appliable mixin rejects the whole write unless the caller opts out.
fn default_true() -> bool {
    true
}

/// One `ensure_mixins` entry's outcome, reported on the success response so a
/// LENIENT skip is never silent
/// ([[spec - ensure-mixin write directive - a governed write ensures a type-claim mixin idempotently, folded into the write's own commit]]).
#[derive(Debug, Serialize)]
struct MixinOutcome {
    mixin: String,
    /// `applied`, `no_op`, or `skipped`.
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl MixinOutcome {
    fn applied(mixin: &str) -> Self {
        Self {
            mixin: mixin.to_string(),
            outcome: "applied",
            reason: None,
        }
    }
    fn no_op(mixin: &str) -> Self {
        Self {
            mixin: mixin.to_string(),
            outcome: "no_op",
            reason: None,
        }
    }
    fn skipped(mixin: &str, reason: String) -> Self {
        Self {
            mixin: mixin.to_string(),
            outcome: "skipped",
            reason: Some(reason),
        }
    }
}

/// Fold a write's `ensure_mixins` into the just-written file, in order, AFTER the
/// stamps (so a mixin validates against the stamped content), returning the
/// response hash and each entry's outcome. Runs INSIDE the write closure, so an
/// applied mixin shares the write's commit. Under STRICT an un-appliable mixin
/// propagates a `MutationReject`, so the saga compensates and nothing lands;
/// under LENIENT it is a reported skip and the write proceeds.
fn fold_write_mixins(
    handle: &EngineHandle,
    target: &Path,
    mixins: &[String],
    strict: bool,
    primary: crate::ir::ContentHash,
) -> Result<(crate::ir::ContentHash, Vec<MixinOutcome>), crate::mutate::MutationReject> {
    let mut hash = primary;
    let mut outcomes = Vec::new();
    for mixin in mixins {
        // Re-read the just-written bytes, so a second mixin sees the first's claim.
        let content = std::fs::read_to_string(target).map_err(|e| {
            crate::mutate::MutationReject::new(format!(
                "ensure_mixin could not read {} to fold into: {e}",
                target.display()
            ))
        })?;
        let decision =
            match handle.read(|kb| crate::ensure_mixin::decide(kb, target, &content, mixin)) {
                Read::Ready { value, .. } => value,
                _ => {
                    return Err(crate::mutate::MutationReject::new(
                        "engine is not ready to resolve an ensure_mixin",
                    ))
                }
            };
        match decision {
            crate::ensure_mixin::MixinDecision::NoOp => outcomes.push(MixinOutcome::no_op(mixin)),
            crate::ensure_mixin::MixinDecision::Apply(new_content) => {
                std::fs::write(target, &new_content).map_err(|e| {
                    crate::mutate::MutationReject::new(format!(
                        "ensure_mixin could not write {}: {e}",
                        target.display()
                    ))
                })?;
                hash = crate::ir::ContentHash::of(new_content.as_bytes());
                outcomes.push(MixinOutcome::applied(mixin));
            }
            crate::ensure_mixin::MixinDecision::UnAppliable(reason) => {
                if strict {
                    return Err(crate::mutate::MutationReject::new(format!(
                        "ensure_mixin '{mixin}' cannot be applied cleanly and ensure_mixins_strict is set: {reason}"
                    )));
                }
                outcomes.push(MixinOutcome::skipped(mixin, reason));
            }
        }
    }
    Ok((hash, outcomes))
}

/// Whether a `stamps` / `ensure_mixins` rider may ride this write's target, and
/// the reject reason when it may not. A rider splices an INSTANCE's frontmatter:
/// a `type:`-first record for a stamp, a mixin on the identity claim. A type-def
/// cannot carry that:
/// - a type-def (`*.type.yaml` / `*.type.yml`): its `type:` is a
///   PARENT claim, not an instance identity, and a stamp's `---` wrap would strand
///   the def's body past a document separator. `rename` already rejects a type-def
///   target; the frontmatter verbs do so here.
///
/// A plain instance file returns `None` and the rider rides it.
fn rider_target_reject(write_path: &Path) -> Option<crate::mutate::MutationReject> {
    if au_parser::classify_by_path(write_path) == Some(au_parser::FileKind::TypeDef) {
        return Some(crate::mutate::MutationReject::new(
            "a stamp or ensure_mixin targets an instance's frontmatter, not a type-def; a \
             type-def's `type:` is a parent claim, not an instance identity",
        ));
    }
    None
}

/// Project mixin outcomes into a mutation response's `extra` map, under
/// `ensure_mixins`, omitted when the write carried none.
fn mixin_outcomes_extra(outcomes: Vec<MixinOutcome>) -> serde_json::Map<String, serde_json::Value> {
    let mut extra = serde_json::Map::new();
    if !outcomes.is_empty() {
        extra.insert(
            "ensure_mixins".into(),
            serde_json::to_value(outcomes).expect("mixin outcomes serialize"),
        );
    }
    extra
}

/// Run one mutation: resolve and guard the path, execute the write, drive
/// the synchronous rebuild, then answer with fresh state — the post-mutation
/// version, the file's catalog hash, and its diagnostics. A rejection is an
/// `error` frame and writes nothing; diagnostics never reject, they ride
/// the response ([[spec - mutation channel v1 - a closed primitive catalog
/// through one mediated path]]).
async fn handle_mutation(handle: &EngineHandle, m: Mutation) -> serde_json::Value {
    // Serialize the write pipeline: one mutation at a time across every
    // connection. Held across the whole saga and rebuild, this enforces the
    // single-threaded model the intent marker and crash recovery rely on. Reads
    // and subscriptions never take it.
    let _write = handle.write_lock().lock().await;
    // Mutating while Deriving would race the initial build; the consumer
    // waits for ready like it does for reads.
    if handle.version().is_none() {
        return serde_json::to_value(Response::not_ready()).expect("response serializes");
    }
    match m {
        Mutation::WriteFile(args) => {
            let write_path = match resolve_mutation_target(handle, &args.path) {
                Ok(t) => t,
                Err(frame) => return frame,
            };
            let response_path = write_path.clone();
            // A stamp or a mixin rides an instance file's frontmatter; a
            // type-def target would be corrupted. Reject early, before any write.
            if !args.stamps.is_empty() || !args.ensure_mixins.is_empty() {
                if let Some(reject) = rider_target_reject(&write_path) {
                    return mutation_reject_frame(reject);
                }
            }
            let attribution = match resolve_attribution(&args.attribution) {
                Ok(a) => a,
                Err(frame) => return frame,
            };
            let mut plan = resolve_saga_plan(handle, &[write_path.clone()]);
            plan.attribution = attribution;
            let marker_dir = saga_marker_dir(handle.root());
            let summary = format!("write_file {}", args.path);
            // Disk write, commit-or-compensate, then full rebuild, off the workers.
            let blocking_handle = handle.clone();
            let blocking_path = write_path.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let ((written, mixins), commits) =
                    apply_saga(&ShellGit, &marker_dir, &plan, summary, false, || {
                        let written = crate::mutate::write_file(
                            &blocking_path,
                            &args.content,
                            args.expected_hash.as_deref(),
                        )?;
                        // Fold the stamps then the mixins into the just-written file,
                        // so they share this commit. The final hash (if a rider
                        // changed the file) is the response hash, so `reflected`
                        // compares the riddled bytes.
                        let written = fold_write_stamps(&blocking_path, &args.stamps, written)?;
                        fold_write_mixins(
                            &blocking_handle,
                            &blocking_path,
                            &args.ensure_mixins,
                            args.ensure_mixins_strict,
                            written,
                        )
                    })?;
                blocking_handle.rebuild_paths(saga_dirty_set(&plan));
                Ok::<_, crate::mutate::MutationReject>((written, mixins, commits))
            })
            .await
            .expect("mutation task never panics");
            match outcome {
                Err(reject) => mutation_reject_frame(reject),
                Ok((written, mixins, commits)) => mutation_response_extra(
                    handle,
                    &response_path,
                    Some(written),
                    commits,
                    mixin_outcomes_extra(mixins),
                ),
            }
        }
        Mutation::EditFile(args) => {
            let write_path = match resolve_mutation_target(handle, &args.path) {
                Ok(t) => t,
                Err(frame) => return frame,
            };
            let response_path = write_path.clone();
            if !args.stamps.is_empty() || !args.ensure_mixins.is_empty() {
                if let Some(reject) = rider_target_reject(&write_path) {
                    return mutation_reject_frame(reject);
                }
            }
            let attribution = match resolve_attribution(&args.attribution) {
                Ok(a) => a,
                Err(frame) => return frame,
            };
            let mut plan = resolve_saga_plan(handle, &[write_path.clone()]);
            plan.attribution = attribution;
            let marker_dir = saga_marker_dir(handle.root());
            let summary = format!("edit_file {}", args.path);
            let blocking_handle = handle.clone();
            let blocking_path = write_path.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let ((written, mixins), commits) =
                    apply_saga(&ShellGit, &marker_dir, &plan, summary, false, || {
                        let written = crate::mutate::edit_file(
                            &blocking_path,
                            &args.old_string,
                            &args.new_string,
                            args.replace_all,
                        )?;
                        let written = fold_write_stamps(&blocking_path, &args.stamps, written)?;
                        fold_write_mixins(
                            &blocking_handle,
                            &blocking_path,
                            &args.ensure_mixins,
                            args.ensure_mixins_strict,
                            written,
                        )
                    })?;
                blocking_handle.rebuild_paths(saga_dirty_set(&plan));
                Ok::<_, crate::mutate::MutationReject>((written, mixins, commits))
            })
            .await
            .expect("mutation task never panics");
            match outcome {
                Err(reject) => mutation_reject_frame(reject),
                Ok((written, mixins, commits)) => mutation_response_extra(
                    handle,
                    &response_path,
                    Some(written),
                    commits,
                    mixin_outcomes_extra(mixins),
                ),
            }
        }
        Mutation::DeleteFile(args) => {
            let write_path = match resolve_mutation_target(handle, &args.path) {
                Ok(t) => t,
                Err(frame) => return frame,
            };
            let response_path = write_path.clone();
            let attribution = match resolve_attribution(&args.attribution) {
                Ok(a) => a,
                Err(frame) => return frame,
            };
            let mut plan = resolve_saga_plan(handle, &[write_path.clone()]);
            plan.attribution = attribution;
            let marker_dir = saga_marker_dir(handle.root());
            let summary = format!("delete_file {}", args.path);
            let blocking_handle = handle.clone();
            let blocking_path = write_path.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                // The last-live commit: HEAD immediately BEFORE the delete is the
                // parent of the deletion commit, the last commit where the file
                // still existed. The clean-at-HEAD precondition guarantees the
                // file was present there, and the serialized write lock keeps
                // this HEAD the deletion commit's parent. Captured before the
                // saga runs, since the commit advances HEAD. au-provenance's
                // delete tombstone pins this so the file's last content reads
                // back, see [[spec - git write path - commit-per-mutation as a
                // local saga over the workspace's materialized repos]].
                let last_live = crate::gitwriter::head_commit_fast(&blocking_path);
                let ((), commits) =
                    apply_saga(&ShellGit, &marker_dir, &plan, summary, false, || {
                        crate::mutate::delete_file(&blocking_path, args.expected_hash.as_deref())
                    })?;
                blocking_handle.rebuild_paths(saga_dirty_set(&plan));
                Ok::<_, crate::mutate::MutationReject>((commits, last_live))
            })
            .await
            .expect("mutation task never panics");
            match outcome {
                Err(reject) => mutation_reject_frame(reject),
                // No content landed, so no written hash to lag; the response
                // carries the now-absent target's fresh (empty) state.
                Ok((commits, last_live)) => {
                    // Surface the last-live commit only when the delete actually
                    // committed (on-git): off-git there is no deletion commit and
                    // no last-live to pin. The deletion commit itself stays in
                    // `commits` / `commit` for attribution.
                    let mut extra = serde_json::Map::new();
                    if !commits.is_empty() {
                        if let Some(sha) = last_live {
                            extra.insert(
                                "last_live_commit".to_string(),
                                serde_json::Value::String(sha),
                            );
                        }
                    }
                    mutation_response_extra(handle, &response_path, None, commits, extra)
                }
            }
        }
        Mutation::AssignBlockId(args) => assign_block_id(handle, args).await,
        Mutation::EditRecord(args) => handle_edit_record(handle, args).await,
        Mutation::AppendRecord(args) => handle_append_record(handle, args).await,
        Mutation::Rename(args) => {
            let from = match resolve_mutation_target(handle, &args.path) {
                Ok(t) => t,
                Err(frame) => return frame,
            };
            let to = match resolve_mutation_target(handle, &args.to) {
                Ok(t) => t,
                Err(frame) => return frame,
            };
            // `rename` moves content/instance files. A type-def's name derives
            // from its filename, so moving one renames the type and strands its
            // claims — and the claims are not wikilinks, so the rewrite here
            // would not touch them. That is `rename_type`'s job, not this.
            if au_parser::classify_by_path(&from) == Some(au_parser::FileKind::TypeDef)
                || au_parser::classify_by_path(&to) == Some(au_parser::FileKind::TypeDef)
            {
                return mutation_reject_frame(crate::mutate::MutationReject::new(
                    "rename does not move type-def files — a type-def's name derives from its \
                     filename; use rename_type",
                ));
            }
            // Over the held knowledge base: the same-repo guard (v1 moves within one
            // repo), and the inbound referrers to rewrite — each source and the
            // byte spans of its references to `from`, grouped by source. The
            // index locates; the source on disk is the truth, re-read in the
            // saga. A self-reference is included, so the moved file points at
            // its new name.
            let gathered = handle.read(|v| {
                let (rf, rt) = match (v.repos.repo_of(&from), v.repos.repo_of(&to)) {
                    (Some(rf), Some(rt)) => (rf, rt),
                    _ => return Err("path or destination escapes the workspace".to_string()),
                };
                if rf.root != rt.root {
                    return Err(
                        "cross-repo rename is not supported yet — path and to must be in the same repo"
                            .to_string(),
                    );
                }
                // The repo-relative paths the references resolve against, so the
                // rewrite can preserve each referrer's spelling mode.
                let old_rel = from.strip_prefix(&rf.root).unwrap_or(&from).to_path_buf();
                let new_rel = to.strip_prefix(&rf.root).unwrap_or(&to).to_path_buf();
                let mut by_source: BTreeMap<PathBuf, Vec<au_diagnostics::ByteRange>> =
                    BTreeMap::new();
                let mut referrer_hashes: BTreeMap<PathBuf, Option<crate::ir::ContentHash>> =
                    BTreeMap::new();
                for bl in v.backlinks(&from) {
                    by_source.entry(bl.source.clone()).or_default().push(bl.span);
                    referrer_hashes
                        .entry(bl.source.clone())
                        .or_insert_with(|| v.catalog.get(&bl.source).and_then(|e| e.hash));
                }
                Ok((rf.root.clone(), old_rel, new_rel, by_source, referrer_hashes))
            });
            let (move_repo_root, old_rel, new_rel, referrers, referrer_hashes) = match gathered {
                crate::Read::Ready { value: Ok(g), .. } => g,
                crate::Read::Ready {
                    value: Err(msg), ..
                } => {
                    return mutation_reject_frame(crate::mutate::MutationReject::new(msg));
                }
                crate::Read::NotReady => {
                    return serde_json::to_value(Response::not_ready())
                        .expect("response serializes");
                }
            };
            // The mutation touches `from`, `to`, and every referrer. `from` is a
            // referrer too when the file references itself; it is already in the
            // touched set via the move, so it is not added twice.
            let mut touched = vec![from.clone(), to.clone()];
            touched.extend(referrers.keys().filter(|p| **p != from).cloned());
            let not_written = referrers_not_written(&referrers);
            // A referrer this rename would rewrite that lives in a consumed member
            // rejects the whole refactor (GAP 2).
            if let Some(reject) =
                reject_if_consumed_referrers(handle, &written_referrers(&referrers, &not_written))
            {
                return mutation_reject_frame(reject);
            }
            let mut plan = resolve_saga_plan(handle, &touched);
            plan.exempt_from_clean_check = not_written;
            let marker_dir = saga_marker_dir(handle.root());
            let summary = format!("rename {} -> {}", args.path, args.to);
            let blocking_handle = handle.clone();
            let blocking_from = from.clone();
            let blocking_to = to.clone();
            let stamps = args.stamps;
            let ensure_mixins = args.ensure_mixins;
            let strict = args.ensure_mixins_strict;
            let outcome = tokio::task::spawn_blocking(move || {
                // The move belongs to the tree where the file actually moved, so
                // only that member's commit carries the `Moved:` trailer; a
                // referrer in another tree only had links rewritten.
                let moves = moves_for(&move_repo_root, &old_rel, &new_rel);
                let ((written, mixins), commits) = apply_saga_with_moves(
                    &ShellGit,
                    &marker_dir,
                    &plan,
                    summary,
                    &moves,
                    true,
                    || {
                        // Rewrite every referrer first, then move the file.
                        for (path, spans) in &referrers {
                            let content = read_referrer_checked(
                                path,
                                referrer_hashes.get(path).copied().flatten(),
                            )?;
                            let rewritten =
                                crate::rename::rewrite_links(&content, spans, &old_rel, &new_rel)?;
                            // A referrer whose every reference is commit-pinned
                            // yields no edits, so the rewrite is byte-identical.
                            // Writing it anyway costs a disk write and wakes the
                            // watcher for a file that did not change.
                            if rewritten != content {
                                std::fs::write(path, rewritten).map_err(|e| {
                                    crate::mutate::MutationReject::new(format!(
                                        "cannot write referrer {}: {e}",
                                        path.display()
                                    ))
                                })?;
                            }
                        }
                        let written = crate::mutate::rename_file(&blocking_from, &blocking_to)?;
                        // Fold the stamps then the mixins into the RENAMED file,
                        // after the move, so they ride the rename's commit.
                        let written = fold_write_stamps(&blocking_to, &stamps, written)?;
                        fold_write_mixins(
                            &blocking_handle,
                            &blocking_to,
                            &ensure_mixins,
                            strict,
                            written,
                        )
                    },
                )?;
                blocking_handle.rebuild_paths(saga_dirty_set(&plan));
                Ok::<_, crate::mutate::MutationReject>((written, mixins, commits))
            })
            .await
            .expect("mutation task never panics");
            match outcome {
                Err(reject) => mutation_reject_frame(reject),
                Ok((written, mixins, commits)) => mutation_response_extra(
                    handle,
                    &to,
                    Some(written),
                    commits,
                    mixin_outcomes_extra(mixins),
                ),
            }
        }
        Mutation::RenameType(args) => {
            // A rename_type cascades across the def file and every referrer, so it
            // has NO single target file for a stamp. It rejects any stamp here; when
            // the multi-file `stamp.path` (naming one file in the write set) lands,
            // rename_type gains stamping with an explicit target.
            if !args.stamps.is_empty() {
                return mutation_reject_frame(crate::mutate::MutationReject::new(
                    "rename_type has no single target file for a stamp — it rewrites the def and \
                     every referrer; stamping a specific file needs the deferred multi-file \
                     stamp target",
                ));
            }
            // The old name may be `base::repo` to select WHICH owner's identity to
            // rename when two mounted repos own the name; bare picks the first
            // owner. The new name is always the owner's own, so it stays bare.
            let (old_base, old_repo) = match args.old_name.split_once("::") {
                Some((base, repo)) => (base.to_string(), Some(repo.to_string())),
                None => (args.old_name.clone(), None),
            };
            let old_name = au_core::TypeName(old_base);
            let new_name = au_core::TypeName(args.new_name.clone());
            // Over the held knowledge base: the owned def file, the new path, and both
            // reference surfaces. Surface 1 (type-name references) is repo-local
            // and its edits derive from each referrer's parse (claim spans, raw
            // shapes). Surface 2 (wikilinks to the def file) is the backlink
            // index, mounted-set scope, cross-repo referrers included. Each
            // referrer's content hash is held so the write-time re-read is
            // guarded.
            let gathered = handle.read(|v| {
                let def =
                    find_owned_type_def(v, &old_name, old_repo.as_deref()).ok_or_else(|| {
                        match &old_repo {
                            Some(r) => format!(
                                "no type-def named '{}' owned by '{}' to rename",
                                old_name.as_str(),
                                r
                            ),
                            None => {
                                format!("no type-def named '{}' to rename", old_name.as_str())
                            }
                        }
                    })?;
                let from = def.source_path.clone();
                let repo = v
                    .repos
                    .repo_of(&from)
                    .ok_or_else(|| "the type-def's repo is not mounted".to_string())?;
                let repo_name = repo.name.clone();
                let repo_root = repo.root.clone();
                // The def file is a PRIMARY write derived from the graph, not a path
                // arg, so it bypasses `resolve_editable_target`. Gate it here: a
                // type-def owned by a consumed (`discover` / `dep`) member is not the
                // engine's to author.
                let role = crate::wire::member_role(v, &repo_name);
                if !role.editable() {
                    return Err(format!(
                        "type-def '{}' is owned by '{}', mounted as '{}', not editable — \
                         list it in workspace.yaml 'edit:' to rename its types",
                        old_name.as_str(),
                        repo_name.as_str(),
                        role.as_str(),
                    ));
                }
                if !au_core::is_valid_type_name(new_name.as_str()) {
                    return Err(format!("'{}' is not a valid type name", new_name.as_str()));
                }
                if v.graphs.of(&repo_name).get(&new_name).is_some() {
                    return Err(format!(
                        "type-def '{}' already exists — pick a free name",
                        new_name.as_str()
                    ));
                }
                let to = renamed_type_def_path(&from, &new_name).ok_or_else(|| {
                    format!("could not derive the new path for '{}'", new_name.as_str())
                })?;
                let old_rel = from.strip_prefix(&repo_root).unwrap_or(&from).to_path_buf();
                let new_rel = to.strip_prefix(&repo_root).unwrap_or(&to).to_path_buf();

                let mut type_edits: BTreeMap<PathBuf, Vec<(au_diagnostics::ByteRange, String)>> =
                    BTreeMap::new();
                let mut referrer_hashes: BTreeMap<PathBuf, Option<crate::ir::ContentHash>> =
                    BTreeMap::new();
                for path in v.type_referrers(&repo_name, &old_name) {
                    if let Some(entry) = v.catalog.get(&path) {
                        // Each referrer rewrites against its OWN repo: a bare `foo`
                        // in the def's repo becomes bare `new`, a mounted
                        // `foo::<def repo>` in a peer becomes `new::<def repo>`,
                        // qualifier-preserving.
                        let file_repo = crate::refnames::source_repo(&v.repos, &path);
                        let edits = crate::typerefs::type_ref_edits(
                            &entry.parse,
                            file_repo.as_str(),
                            &old_name,
                            repo_name.as_str(),
                            &new_name,
                        );
                        if !edits.is_empty() {
                            referrer_hashes.entry(path.clone()).or_insert(entry.hash);
                            type_edits.insert(path, edits);
                        }
                    }
                }

                let mut link_spans: BTreeMap<PathBuf, Vec<au_diagnostics::ByteRange>> =
                    BTreeMap::new();
                for bl in v.backlinks(&from) {
                    link_spans
                        .entry(bl.source.clone())
                        .or_default()
                        .push(bl.span);
                    referrer_hashes
                        .entry(bl.source.clone())
                        .or_insert_with(|| v.catalog.get(&bl.source).and_then(|e| e.hash));
                }

                // Every referrer to rewrite, deduped across both surfaces.
                let referrers: Vec<PathBuf> = type_edits
                    .keys()
                    .chain(link_spans.keys())
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();

                Ok(RenameTypeGather {
                    from,
                    to,
                    move_repo_root: repo_root,
                    old_rel,
                    new_rel,
                    type_edits,
                    link_spans,
                    referrer_hashes,
                    referrers,
                })
            });
            let RenameTypeGather {
                from,
                to,
                move_repo_root,
                old_rel,
                new_rel,
                type_edits,
                link_spans,
                referrer_hashes,
                referrers,
            } = match gathered {
                crate::Read::Ready { value: Ok(g), .. } => g,
                crate::Read::Ready {
                    value: Err(msg), ..
                } => {
                    return mutation_reject_frame(crate::mutate::MutationReject::new(msg));
                }
                crate::Read::NotReady => {
                    return serde_json::to_value(Response::not_ready())
                        .expect("response serializes");
                }
            };
            // The paths the saga touches: the def's own file (`from` plus the
            // not-yet-existing `to`), and each referrer. Deduped.
            let mut touched_set: std::collections::BTreeSet<PathBuf> =
                std::collections::BTreeSet::new();
            touched_set.insert(from.clone());
            touched_set.insert(to.clone());
            for path in &referrers {
                touched_set.insert(path.clone());
            }
            let touched: Vec<PathBuf> = touched_set.into_iter().collect();
            let mut plan = resolve_saga_plan(handle, &touched);
            // `rename_type` writes a referrer for either of two reasons, so being
            // left unwritten needs both to be absent: a type-name CLAIM edit
            // rewrites the file even when every wikilink in it is frozen.
            {
                let mut exempt: std::collections::BTreeSet<PathBuf> =
                    std::collections::BTreeSet::new();
                let mut written: std::collections::BTreeSet<PathBuf> =
                    std::collections::BTreeSet::new();
                for path in &referrers {
                    let claim_edit = type_edits.get(path).is_some_and(|e| !e.is_empty());
                    // Unreadable reads as WRITTEN, so it stays gated: guessing
                    // exempt would write over uncommitted work unchecked.
                    let link_edit = link_spans.get(path).is_some_and(|spans| {
                        read_referrer_checked(path, referrer_hashes.get(path).copied().flatten())
                            .map(|c| crate::rename::would_rewrite(&c, spans))
                            .unwrap_or(true)
                    });
                    if claim_edit || link_edit {
                        written.insert(path.clone());
                    } else {
                        exempt.insert(path.clone());
                    }
                }
                exempt.retain(|p| !written.contains(p));
                // A referrer this rename_type would rewrite (a claim edit or a live
                // wikilink) that lives in a consumed member rejects the whole
                // refactor (GAP 2). `written` is referrers only; the def file's own
                // editability is gated earlier (GAP 1).
                if let Some(reject) = reject_if_consumed_referrers(handle, &written) {
                    return mutation_reject_frame(reject);
                }
                plan.exempt_from_clean_check = exempt;
            }
            let marker_dir = saga_marker_dir(handle.root());
            let summary = format!("rename_type {} -> {}", old_name.as_str(), new_name.as_str());
            // A rename records the def file's move for the pinned-reference
            // forward trace, scoped to the def's repo.
            let moves = moves_for(&move_repo_root, &old_rel, &new_rel);
            let blocking_handle = handle.clone();
            let blocking_from = from.clone();
            let blocking_to = to.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let (written, commits) = apply_saga_with_moves(
                    &ShellGit,
                    &marker_dir,
                    &plan,
                    summary,
                    &moves,
                    true,
                    || {
                        // Rewrite every referrer (both surfaces merged) first, then
                        // relocate the def. A self-referencing def is rewritten under
                        // its old name, then the file move carries it.
                        for path in &referrers {
                            rewrite_referrer(
                                path,
                                referrer_hashes.get(path).copied().flatten(),
                                type_edits.get(path),
                                link_spans.get(path),
                                &old_rel,
                                &new_rel,
                            )?;
                        }
                        crate::mutate::rename_file(&blocking_from, &blocking_to)
                    },
                )?;
                blocking_handle.rebuild_paths(saga_dirty_set(&plan));
                Ok::<_, crate::mutate::MutationReject>((written, commits))
            })
            .await
            .expect("mutation task never panics");
            match outcome {
                Err(reject) => mutation_reject_frame(reject),
                Ok((written, commits)) => mutation_response(handle, &to, Some(written), commits),
            }
        }
        Mutation::Promote(args) => {
            let from = match resolve_mutation_target(handle, &args.path) {
                Ok(t) => t,
                Err(frame) => return frame,
            };
            let to = match resolve_mutation_target(handle, &args.to) {
                Ok(t) => t,
                Err(frame) => return frame,
            };
            if au_parser::classify_by_path(&to) == Some(au_parser::FileKind::TypeDef) {
                return mutation_reject_frame(crate::mutate::MutationReject::new(
                    "promote creates an instance file, not a type-def — `to` must not be a \
                     type-def path",
                ));
            }
            // The `to` extension is the format selector and must be one the engine
            // writes an instance into: `.md` (frontmatter-fenced, empty body) or
            // `.yaml`/`.yml` (bare YAML). Anything else (no extension, `.txt`) is a
            // rejected request, not a silent default.
            let to_ext = to
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(to_ext.as_str(), "md" | "markdown" | "yaml" | "yml") {
                return mutation_reject_frame(crate::mutate::MutationReject::new(format!(
                    "promote target must be a .md, .markdown, .yaml, or .yml file, got {:?}",
                    args.to
                )));
            }
            // The record locator: a byte offset, or a `^:` id. Exactly one — the
            // offset addresses a record with no id, the id is the stable handle
            // for one that has it (the referenced case always does).
            enum Locator {
                At(usize),
                Block(String),
            }
            let locator = match (args.at, &args.block_id) {
                (Some(_), Some(_)) => {
                    return mutation_reject_frame(crate::mutate::MutationReject::new(
                        "pass exactly one of `at` or `block_id`, not both",
                    ));
                }
                (None, None) => {
                    return mutation_reject_frame(crate::mutate::MutationReject::new(
                        "pass exactly one of `at` (a byte offset) or `block_id`",
                    ));
                }
                (Some(at), None) => Locator::At(at),
                (None, Some(id)) => Locator::Block(id.clone()),
            };
            // Over the held knowledge base: the same-repo guard, the record's whole span,
            // the host's content hash (read-before-write), and the inbound
            // referrers filtered to this record's block-id. A referrer inside the
            // record's own span is excluded — it travels with the extracted
            // content, it is not rewritten in place.
            struct Located {
                record_span: au_diagnostics::ByteRange,
                host_hash: Option<crate::ir::ContentHash>,
                new_stem: String,
                /// The promoted record's own `^:` id, if it has one. The single id
                /// that collapses to a whole-file `[[newFile]]` reference; every
                /// other travelling (nested) id keeps its id under the new file.
                promoted_id: Option<String>,
                referrers: BTreeMap<PathBuf, Vec<au_diagnostics::ByteRange>>,
                /// The held content hash of each referrer, for the read-before-write
                /// drift check when the saga re-reads it.
                referrer_hashes: BTreeMap<PathBuf, Option<crate::ir::ContentHash>>,
                /// References inside the record to any block-id that travels with
                /// it (its own id or a nested one), host-absolute. Redirected onto
                /// the new file during extraction so they do not dangle once the
                /// record moves.
                inner_self_pointers: Vec<au_diagnostics::ByteRange>,
            }
            let gathered = handle.read(|v| {
                let (rf, rt) = match (v.repos.repo_of(&from), v.repos.repo_of(&to)) {
                    (Some(rf), Some(rt)) => (rf, rt),
                    _ => return Err("path or destination escapes the workspace".to_string()),
                };
                if rf.root != rt.root {
                    return Err(
                        "promote into another repo is not supported — path and to must be in the \
                         same repo"
                            .to_string(),
                    );
                }
                let entry = v
                    .catalog
                    .get(&from)
                    .ok_or_else(|| "the host file is not in the knowledge base".to_string())?;
                let inst = match entry.parse.as_ref() {
                    FileParse::Instance {
                        instance: Some(inst),
                        ..
                    } => inst,
                    _ => {
                        return Err("the host file is not a typed instance — nothing to promote"
                            .to_string())
                    }
                };
                let hit = match &locator {
                    Locator::At(at) => record_at(&inst.fields, *at).ok_or_else(|| {
                        "no inline record encloses the offset — pass an offset inside a `^:` record"
                            .to_string()
                    })?,
                    Locator::Block(id) => record_by_block_id(&inst.fields, id)
                        .ok_or_else(|| format!("no inline record carries the block-id `{id}`"))?,
                };
                let record_span = hit.span;
                // The host slot must admit a reference: promote replaces the
                // record with a `[[newFile]]` reference, which a bare inline-only
                // slot (`T`, `any`, a non-`&` compound) cannot hold. Only `&`
                // admits both forms. (A `*` slot holds no inline record, so the
                // locator never finds one there; only the bare case reaches here.)
                if crate::resolution_build::record_slot_admits_reference_at_of_kb(
                    v,
                    &from,
                    inst,
                    record_span,
                ) == Some(false)
                {
                    return Err(
                        "promote out of an inline-only slot: the slot holds an inline record but \
                         cannot hold the `[[newFile]]` reference promote writes — only an \
                         inline-or-reference (`&`) slot can be promoted"
                            .to_string(),
                    );
                }
                // A record is a subtree: the unit that travels to the new file is
                // its whole block-id set — its own `^:` id (if any) plus every
                // nested `^:` id declared within its span. Referrers to any of
                // them must follow, or they dangle at the host the record left.
                let travelling: std::collections::BTreeSet<String> =
                    crate::resolution_build::record_targets_of_kb(v, &from, inst)
                        .into_iter()
                        .filter(|(_, t)| {
                            t.span.start >= record_span.start && t.span.end <= record_span.end
                        })
                        .map(|(id, _)| id)
                        .collect();
                // The host's inbound edges to any travelling id. Empty for a
                // record with no addressable id and no referrers — the
                // referrerless case, cross-repo-safe by construction. An edge
                // inside the record's own span is a self-reference: it travels
                // with the extracted content, so it is redirected there rather
                // than rewritten in place.
                let mut referrers: BTreeMap<PathBuf, Vec<au_diagnostics::ByteRange>> =
                    BTreeMap::new();
                let mut referrer_hashes: BTreeMap<PathBuf, Option<crate::ir::ContentHash>> =
                    BTreeMap::new();
                let mut inner_self_pointers: Vec<au_diagnostics::ByteRange> = Vec::new();
                // A bare `^id` referrer that fills a slot is navigational — the
                // HOST file is its value, `^id` a jump anchor. promote's rewrite
                // follows the extracted record, which would silently change that
                // value from the host to the record, so it is rejected. The author
                // must first make it `^^id` (to follow the record) or drop the
                // anchor. A `^^id` referrer and a slot-less prose bare `^id` rewrite
                // cleanly.
                let mut ambiguous: Option<(PathBuf, String, String)> = None;
                for bl in v.backlinks(&from) {
                    let Some(block) = bl.block_id.as_ref() else {
                        continue;
                    };
                    if !travelling.contains(block.id.as_str()) {
                        continue;
                    }
                    let inside_record = bl.source == from
                        && bl.span.start >= record_span.start
                        && bl.span.end <= record_span.end;
                    if inside_record {
                        inner_self_pointers.push(bl.span);
                    } else {
                        if ambiguous.is_none() && !block.referent {
                            if let Some(slot) = &bl.slot {
                                ambiguous =
                                    Some((bl.source.clone(), slot.clone(), block.id.clone()));
                            }
                        }
                        referrers
                            .entry(bl.source.clone())
                            .or_default()
                            .push(bl.span);
                        referrer_hashes
                            .entry(bl.source.clone())
                            .or_insert_with(|| v.catalog.get(&bl.source).and_then(|e| e.hash));
                    }
                }
                if let Some((src, slot, id)) = ambiguous {
                    return Err(format!(
                        "promote would silently change a reference's value: {} fills slot `{slot}` \
                         with a bare `^{id}` (navigational — the host file is the value, `^{id}` a \
                         jump anchor). Change it to `^^{id}` to follow the promoted record, or drop \
                         the `^{id}` anchor, then retry",
                        src.display()
                    ));
                }
                let new_stem = to
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Ok(Located {
                    record_span,
                    host_hash: entry.hash,
                    new_stem,
                    promoted_id: hit.existing.clone(),
                    referrers,
                    referrer_hashes,
                    inner_self_pointers,
                })
            });
            let located = match gathered {
                crate::Read::Ready { value: Ok(g), .. } => g,
                crate::Read::Ready {
                    value: Err(msg), ..
                } => {
                    return mutation_reject_frame(crate::mutate::MutationReject::new(msg));
                }
                crate::Read::NotReady => {
                    return serde_json::to_value(Response::not_ready())
                        .expect("response serializes");
                }
            };
            // Every touched file: the host, the new file, and each referrer (the
            // host is already present when it is also a self-referrer).
            let mut touched = vec![from.clone(), to.clone()];
            touched.extend(located.referrers.keys().filter(|p| **p != from).cloned());
            let not_written = referrers_not_written(&located.referrers);
            // A referrer this promote would rewrite that lives in a consumed member
            // rejects the whole refactor (GAP 2).
            if let Some(reject) = reject_if_consumed_referrers(
                handle,
                &written_referrers(&located.referrers, &not_written),
            ) {
                return mutation_reject_frame(reject);
            }
            let mut plan = resolve_saga_plan(handle, &touched);
            plan.exempt_from_clean_check = not_written;
            let marker_dir = saga_marker_dir(handle.root());
            let summary = format!("promote {} -> {}", args.path, args.to);
            let blocking_handle = handle.clone();
            let blocking_from = from.clone();
            let blocking_to = to.clone();
            let stamps = args.stamps;
            let Located {
                record_span,
                host_hash,
                new_stem,
                promoted_id,
                referrers,
                referrer_hashes,
                inner_self_pointers,
            } = located;
            let outcome = tokio::task::spawn_blocking(move || {
                // A travelling reference repoints at the new file, carrying its
                // `:field` attribution and `::repo` qualifier. The promoted
                // record's own id collapses to a whole-file `[[newFile]]` (block-id
                // and anchor drop); a nested id that travelled keeps its id under
                // the new file (`[[newFile^nestedId]]`).
                let transform = |w: &au_references::WikilinkRef| {
                    let mut nw = w.clone();
                    nw.target = new_stem.clone();
                    nw.anchor = None;
                    if w.block_id_str() == promoted_id.as_deref() {
                        nw.block_id = None;
                    }
                    nw
                };
                let (written, commits) =
                    apply_saga(&ShellGit, &marker_dir, &plan, summary, true, || {
                        if blocking_to.exists() {
                            return Err(crate::mutate::MutationReject::new(format!(
                                "destination already exists: {}",
                                blocking_to.display()
                            )));
                        }
                        let content = std::fs::read_to_string(&blocking_from).map_err(|e| {
                            crate::mutate::MutationReject::new(format!(
                                "cannot read {}: {e}",
                                blocking_from.display()
                            ))
                        })?;
                        // The span came from the held parse; a file that drifted on
                        // disk makes it unreliable. Read-before-write, structurally.
                        if host_hash != Some(crate::ir::ContentHash::of(content.as_bytes())) {
                            return Err(crate::mutate::MutationReject::new(
                            "the file changed since the engine last read it — wait for the rebuild \
                             and retry",
                        ));
                        }
                        // The record's self-references, redirected to the new file so
                        // they do not dangle once the record moves out of the host.
                        let inner_edits =
                            crate::rename::ref_edits(&content, &inner_self_pointers, &transform)?;
                        let raw = crate::promote::extract_record_file(
                            &content,
                            record_span,
                            &inner_edits,
                        )?;
                        // A `.md` instance needs frontmatter fences around the YAML;
                        // a `.yaml`/`.yml` instance is the bare YAML.
                        let is_md = matches!(
                            blocking_to.extension().and_then(|e| e.to_str()),
                            Some("md") | Some("markdown")
                        );
                        let file_content = if is_md {
                            format!("---\n{raw}---\n")
                        } else {
                            raw
                        };
                        // The host's own edits — the record replacement plus any
                        // self-referrer rewrites — applied as one last-first pass so
                        // they never shift each other, whatever their file order.
                        //
                        // The record's value span runs through the field's
                        // terminating newline. The reference must replace only the
                        // record's bytes and LEAVE that newline, or the following
                        // line (the closing `---` fence, or the next key) glues onto
                        // the reference and the host stops parsing. `extract_record_file`
                        // above keeps the full `record_span`, so the extracted file
                        // still terminates cleanly; only the in-host replacement trims.
                        let mut ref_end = record_span.end;
                        while ref_end > record_span.start
                            && matches!(content.as_bytes().get(ref_end - 1), Some(b'\n' | b'\r'))
                        {
                            ref_end -= 1;
                        }
                        let ref_span = au_diagnostics::ByteRange::new(record_span.start, ref_end);
                        let ref_text = format!("\"[[{new_stem}]]\"");
                        let mut host_edits = vec![(ref_span, ref_text)];
                        if let Some(spans) = referrers.get(&blocking_from) {
                            host_edits
                                .extend(crate::rename::ref_edits(&content, spans, &transform)?);
                        }
                        let host_after = crate::rename::apply_edits(&content, host_edits)?;
                        // Rewrite every other referrer.
                        for (source, spans) in &referrers {
                            if *source == blocking_from {
                                continue;
                            }
                            let rc = read_referrer_checked(
                                source,
                                referrer_hashes.get(source).copied().flatten(),
                            )?;
                            let rewritten = crate::rename::rewrite_refs(&rc, spans, &transform)?;
                            // A referrer whose every reference is commit-pinned
                            // yields no edits, so the rewrite is byte-identical.
                            // Writing it anyway costs a disk write and wakes the
                            // watcher for a file that did not change.
                            if rewritten != rc {
                                std::fs::write(source, rewritten).map_err(|e| {
                                    crate::mutate::MutationReject::new(format!(
                                        "cannot write referrer {}: {e}",
                                        source.display()
                                    ))
                                })?;
                            }
                        }
                        // The new file, then the host pointing at it.
                        let written = crate::mutate::write_file(&blocking_to, &file_content, None)?;
                        std::fs::write(&blocking_from, host_after).map_err(|e| {
                            crate::mutate::MutationReject::new(format!(
                                "cannot write host {}: {e}",
                                blocking_from.display()
                            ))
                        })?;
                        // Fold the stamp into the newly-extracted file, this commit.
                        Ok(fold_write_stamps(&blocking_to, &stamps, written)?)
                    })?;
                blocking_handle.rebuild_paths(saga_dirty_set(&plan));
                Ok::<_, crate::mutate::MutationReject>((written, commits))
            })
            .await
            .expect("mutation task never panics");
            match outcome {
                Err(reject) => mutation_reject_frame(reject),
                Ok((written, commits)) => mutation_response(handle, &to, Some(written), commits),
            }
        }
        Mutation::Inline(args) => {
            let file = match resolve_mutation_target(handle, &args.path) {
                Ok(t) => t,
                Err(frame) => return frame,
            };
            let into = match resolve_mutation_target(handle, &args.into) {
                Ok(t) => t,
                Err(frame) => return frame,
            };
            if file == into {
                return mutation_reject_frame(crate::mutate::MutationReject::new(
                    "inline needs a file and a distinct host — `path` and `into` are the same file",
                ));
            }
            // Over the held knowledge base: the same-repo guard, the file's body-emptiness
            // (a record has no body), the chosen reference value in `into`, the
            // host's other references (-> local `[[^id]]`), and the cross-file
            // referrers (-> `[[host^id]]`). Plus the read-before-write hashes.
            struct Located {
                value_span: au_diagnostics::ByteRange,
                into_other: Vec<au_diagnostics::ByteRange>,
                cross: BTreeMap<PathBuf, Vec<au_diagnostics::ByteRange>>,
                /// The held content hash of each cross-file referrer, for the
                /// read-before-write drift check when the saga re-reads it.
                cross_hashes: BTreeMap<PathBuf, Option<crate::ir::ContentHash>>,
                into_hash: Option<crate::ir::ContentHash>,
                file_hash: Option<crate::ir::ContentHash>,
                file_is_md: bool,
                into_stem: String,
                /// The combined file-local id namespace of `file` and `into`. Fresh
                /// ids (the new record id and any collision renames) are drawn clear
                /// of it.
                namespace: std::collections::BTreeSet<String>,
                /// Each id `file`'s subtree shares with `into`, plus its `^:`
                /// declaration span in `file` (file-absolute). Renamed to a fresh id
                /// during the fold so the merged file has no duplicate id.
                colliding: Vec<(String, au_diagnostics::ByteRange)>,
                /// `file`'s references to its own blocks (file-absolute spans),
                /// rebased onto the host as the folded content travels.
                file_self: Vec<au_diagnostics::ByteRange>,
                /// Referrers that cannot be re-pointed to a block-id, one
                /// human-readable reason each — a `file*` whole-file slot or a
                /// `[[file#head]]` anchor. A non-empty list is a rejected request.
                offenders: Vec<String>,
            }
            let gathered = handle.read(|v| {
                let (rf, ri) = match (v.repos.repo_of(&file), v.repos.repo_of(&into)) {
                    (Some(a), Some(b)) => (a, b),
                    _ => return Err("path or into escapes the workspace".to_string()),
                };
                if rf.root != ri.root {
                    return Err(
                        "inline across repos is not supported — path and into must be in the same \
                         repo"
                            .to_string(),
                    );
                }
                let file_entry = v
                    .catalog
                    .get(&file)
                    .ok_or_else(|| "the file to inline is not in the knowledge base".to_string())?;
                let (file_is_md, has_body, file_inst) = match file_entry.parse.as_ref() {
                    FileParse::Instance {
                        is_markdown,
                        body,
                        instance: Some(inst),
                        ..
                    } => (*is_markdown, !body.trim().is_empty(), inst),
                    _ => return Err("the file to inline is not a typed instance".to_string()),
                };
                if has_body {
                    return Err(
                        "the file to inline has a non-empty body — a record has no body; strip the \
                         body first, or keep it as its own file"
                            .to_string(),
                    );
                }
                let into_entry = v
                    .catalog
                    .get(&into)
                    .ok_or_else(|| "the host `into` is not in the knowledge base".to_string())?;
                let into_inst = match into_entry.parse.as_ref() {
                    FileParse::Instance {
                        instance: Some(inst),
                        ..
                    } => inst,
                    _ => return Err("the host `into` is not a typed instance".to_string()),
                };
                // Block-ids are unique within a file. Folding `file`'s subtree into
                // `into` moves every `^:` id `file` declares into `into`'s
                // namespace; an id `into` already declares (inline record or body
                // marker) collides. Rather than reject, the travelling id is
                // renamed to a fresh id during the fold and every reference to it
                // follows — the `colliding` set carries each colliding id and its
                // `^:` declaration span (file-absolute) so the saga can rewrite it.
                // `namespace` is the combined id space the fresh ids are drawn
                // clear of.
                let file_targets =
                    crate::resolution_build::record_targets_of_kb(v, &file, file_inst);
                let file_ids = host_block_ids(file_entry.parse.as_ref(), &file_targets);
                let into_ids = host_block_ids(
                    into_entry.parse.as_ref(),
                    &crate::resolution_build::record_targets_of_kb(v, &into, into_inst),
                );
                let colliding: Vec<(String, au_diagnostics::ByteRange)> = file_ids
                    .intersection(&into_ids)
                    .map(|id| {
                        (
                            id.clone(),
                            file_targets
                                .get(id)
                                .expect("a colliding id is one of file's record ids")
                                .span,
                        )
                    })
                    .collect();
                let namespace: std::collections::BTreeSet<String> =
                    file_ids.union(&into_ids).cloned().collect();
                // `file`'s references to its own blocks travel with the folded
                // content; rebased onto the host (local `[[^id]]`, the renamed id
                // when it collided) so they do not dangle once `file` is deleted.
                // Routed out of `cross` here so the saga rewrites them in the folded
                // text, not on the about-to-be-deleted file.
                let mut into_refs: Vec<au_diagnostics::ByteRange> = Vec::new();
                let mut cross: BTreeMap<PathBuf, Vec<au_diagnostics::ByteRange>> = BTreeMap::new();
                let mut cross_hashes: BTreeMap<PathBuf, Option<crate::ir::ContentHash>> =
                    BTreeMap::new();
                let mut file_self: Vec<au_diagnostics::ByteRange> = Vec::new();
                for bl in v.backlinks(&file) {
                    if bl.source == file {
                        file_self.push(bl.span);
                    } else if bl.source == into {
                        into_refs.push(bl.span);
                    } else {
                        cross.entry(bl.source.clone()).or_default().push(bl.span);
                        cross_hashes
                            .entry(bl.source.clone())
                            .or_insert_with(|| v.catalog.get(&bl.source).and_then(|e| e.hash));
                    }
                }
                if into_refs.is_empty() {
                    return Err(
                        "`into` does not reference `path` — it is not a referrer".to_string()
                    );
                }
                into_refs.sort_by_key(|s| s.start);
                let chosen = match args.at {
                    Some(at) => into_refs
                        .iter()
                        .copied()
                        .find(|s| at >= s.start && at < s.end)
                        .ok_or_else(|| "`at` does not land on a reference to `path`".to_string())?,
                    None => {
                        if into_refs.len() > 1 {
                            return Err(
                                "`into` references `path` more than once — pass `at` to choose \
                                 which reference hosts the record"
                                    .to_string(),
                            );
                        }
                        into_refs[0]
                    }
                };
                let value_span =
                    value_span_at(&into_inst.fields, chosen.start).ok_or_else(|| {
                        "could not locate the reference value to fold into".to_string()
                    })?;
                // Host-slot guard: the chosen reference's slot must admit the
                // inlined record. Inline folds `file`'s content into `into` as a
                // `^:id` record; a reference-only (`*`) slot holds the `[[file]]`
                // reference now but cannot hold the record. Only `&`
                // (inline-or-reference) admits both directions — the dual of
                // promote's bare-slot guard. An undeclared (extra) slot imposes no
                // constraint.
                if let Some(shape) =
                    crate::resolution_build::slot_shape_at_of_kb(v, &into, into_inst, value_span)
                {
                    if !au_core::shape_is_inline_or_reference(&shape) {
                        return Err(format!(
                            "inline into a reference-only slot (`{shape}`): it holds the `[[…]]` \
                             reference but cannot hold the inlined record — only an \
                             inline-or-reference (`&`) slot can be inlined into"
                        ));
                    }
                }
                let into_other: Vec<au_diagnostics::ByteRange> =
                    into_refs.iter().copied().filter(|s| *s != chosen).collect();
                let into_stem = into
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // Referrer guard: every other referrer becomes `[[into^id]]` (or
                // local `[[^id]]` within `into`), addressing the folded block. A
                // referrer that cannot hold a block-id is a rejected request,
                // named in the response `detail`:
                // - a `file*` slot references a whole file; `^block-id` is rejected
                //   on it, see [[type-def shape file::au-type-system]].
                // - a `[[file#head]]` anchor targets a heading in the deleted file.
                let mut offenders: Vec<String> = Vec::new();
                let mut note_offender = |source: &Path, span: au_diagnostics::ByteRange| {
                    let Some(parse) = v.catalog.get(source).map(|e| e.parse.as_ref()) else {
                        return;
                    };
                    if let FileParse::Instance {
                        instance: Some(inst),
                        ..
                    } = parse
                    {
                        // The backlink span covers the `[[…]]` link; the slot
                        // resolver keys on the enclosing scalar's whole value-span,
                        // so map through `value_span_at` first (as the host-slot
                        // guard does for the chosen reference).
                        let governs = value_span_at(&inst.fields, span.start).and_then(|vspan| {
                            crate::resolution_build::slot_shape_at_of_kb(v, source, inst, vspan)
                        });
                        if matches!(governs, Some(Shape::Reference(name)) if name.as_str() == "file") {
                            offenders.push(format!(
                                "{}: a `file*` slot references a whole file and cannot hold a \
                                 block-id",
                                source.display()
                            ));
                            return;
                        }
                    }
                    if let Some(raw) = wikilink_inner_at(parse, span) {
                        if parse_wikilink_inner(&raw).is_ok_and(|w| w.anchor.is_some()) {
                            offenders.push(format!(
                                "{}: a `[[…#head]]` anchor targets a heading in the deleted file",
                                source.display()
                            ));
                        }
                    }
                };
                for span in &into_other {
                    note_offender(&into, *span);
                }
                for (source, spans) in &cross {
                    for span in spans {
                        note_offender(source, *span);
                    }
                }
                Ok(Located {
                    value_span,
                    into_other,
                    cross,
                    cross_hashes,
                    into_hash: into_entry.hash,
                    file_hash: file_entry.hash,
                    file_is_md,
                    into_stem,
                    namespace,
                    colliding,
                    file_self,
                    offenders,
                })
            });
            let located = match gathered {
                crate::Read::Ready { value: Ok(g), .. } => g,
                crate::Read::Ready {
                    value: Err(msg), ..
                } => {
                    return mutation_reject_frame(crate::mutate::MutationReject::new(msg));
                }
                crate::Read::NotReady => {
                    return serde_json::to_value(Response::not_ready())
                        .expect("response serializes");
                }
            };
            let Located {
                value_span,
                into_other,
                cross,
                cross_hashes,
                into_hash,
                file_hash,
                file_is_md,
                into_stem,
                namespace,
                colliding,
                file_self,
                offenders,
            } = located;
            // A referrer that cannot be re-pointed to a block-id is a rejected
            // request, the offenders named in `detail`. The caller resolves them
            // first, or promote stays the safe direction.
            if !offenders.is_empty() {
                return mutation_reject_frame(crate::mutate::MutationReject {
                    message: "inline cannot re-point every referrer to the inlined block — some \
                              referrers reference the whole file or a heading inside it; resolve \
                              them first, or keep the file as its own node"
                        .to_string(),
                    detail: Some(serde_json::json!({ "offenders": offenders })),
                });
            }
            let mut touched = vec![into.clone(), file.clone()];
            touched.extend(cross.keys().filter(|p| **p != into && **p != file).cloned());
            let not_written = referrers_not_written(&cross);
            // A cross-referrer this inline would rewrite that lives in a consumed
            // member rejects the whole refactor (GAP 2).
            if let Some(reject) =
                reject_if_consumed_referrers(handle, &written_referrers(&cross, &not_written))
            {
                return mutation_reject_frame(reject);
            }
            let mut plan = resolve_saga_plan(handle, &touched);
            plan.exempt_from_clean_check = not_written;
            let marker_dir = saga_marker_dir(handle.root());
            let summary = format!("inline {} into {}", args.path, args.into);
            let blocking_handle = handle.clone();
            let blocking_into = into.clone();
            let stamps = args.stamps;
            let blocking_file = file.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let (written, commits) =
                    apply_saga(&ShellGit, &marker_dir, &plan, summary, true, || {
                        let into_content =
                            std::fs::read_to_string(&blocking_into).map_err(|e| {
                                crate::mutate::MutationReject::new(format!(
                                    "cannot read {}: {e}",
                                    blocking_into.display()
                                ))
                            })?;
                        if into_hash != Some(crate::ir::ContentHash::of(into_content.as_bytes())) {
                            return Err(crate::mutate::MutationReject::new(
                            "the host changed since the engine last read it — wait for the rebuild \
                             and retry",
                        ));
                        }
                        let file_content =
                            std::fs::read_to_string(&blocking_file).map_err(|e| {
                                crate::mutate::MutationReject::new(format!(
                                    "cannot read {}: {e}",
                                    blocking_file.display()
                                ))
                            })?;
                        if file_hash != Some(crate::ir::ContentHash::of(file_content.as_bytes())) {
                            return Err(crate::mutate::MutationReject::new(
                            "the file changed since the engine last read it — wait for the rebuild \
                             and retry",
                        ));
                        }
                        // Fresh ids: one per travelling id that collides with the host,
                        // then the new record id — all drawn clear of the combined
                        // namespace and of each other. `colliding` is sorted (a BTreeSet
                        // intersection), so the assignment is deterministic.
                        let mut taken = namespace.clone();
                        let mut rename_map: std::collections::BTreeMap<String, String> =
                            std::collections::BTreeMap::new();
                        let mut file_edits: Vec<(au_diagnostics::ByteRange, String)> = Vec::new();
                        for (old, decl_span) in &colliding {
                            let fresh = crate::mutate::generate_block_id(|c| taken.contains(c));
                            taken.insert(fresh.clone());
                            file_edits.push((*decl_span, fresh.clone()));
                            rename_map.insert(old.clone(), fresh);
                        }
                        let id = crate::mutate::generate_block_id(|c| taken.contains(c));
                        // A reference's block-id under the new home: a colliding id maps
                        // to its fresh rename, any other nested id keeps its id, a
                        // whole-file reference takes the new record id.
                        let map_bid =
                            |b: &Option<au_references::BlockId>| -> Option<au_references::BlockId> {
                                match b {
                                    // A nested id keeps its mode; only the id maps to
                                    // its fresh rename if it collided.
                                    Some(b) => Some(au_references::BlockId {
                                        id: rename_map
                                            .get(&b.id)
                                            .cloned()
                                            .unwrap_or_else(|| b.id.clone()),
                                        referent: b.referent,
                                    }),
                                    // A whole-file record reference now points at the
                                    // inline record, so it must pull the record's VALUE:
                                    // a block-referent `^^id`, not a navigational `^id`
                                    // (which would resolve to the host file instead).
                                    None => Some(au_references::BlockId {
                                        id: id.clone(),
                                        referent: true,
                                    }),
                                }
                            };
                        // `file`'s frontmatter is the record body. Rewrite it before
                        // folding: rename the colliding `^:` declarations, and rebase
                        // `file`'s references to its own blocks to the local form so they
                        // resolve in the host once `file` is gone.
                        let self_form = |w: &au_references::WikilinkRef| {
                            let mut nw = w.clone();
                            nw.target = String::new();
                            nw.block_id = map_bid(&w.block_id);
                            nw.anchor = None;
                            // A local reference has no repo qualifier; a `::`-qualified
                            // self-reference would otherwise serialize to a malformed
                            // empty-target `[[::repo^id]]`.
                            nw.repo = None;
                            nw
                        };
                        file_edits.extend(crate::rename::ref_edits(
                            &file_content,
                            &file_self,
                            &self_form,
                        )?);
                        let file_content = crate::rename::apply_edits(&file_content, file_edits)?;
                        let (frontmatter, has_body) =
                            crate::inline::frontmatter_and_has_body(&file_content, file_is_md)?;
                        if has_body {
                            return Err(crate::mutate::MutationReject::new(
                                "the file to inline has a non-empty body — a record has no body",
                            ));
                        }
                        // The fold, plus the host's other references to the local
                        // `[[^id]]`, applied as one last-first pass.
                        let (fold_span, fold_text) =
                            crate::inline::fold_edit(&into_content, value_span, &frontmatter, &id)?;
                        let local = |w: &au_references::WikilinkRef| {
                            let mut nw = w.clone();
                            nw.target = String::new();
                            nw.block_id = map_bid(&w.block_id);
                            nw.anchor = None;
                            // A local reference drops the repo qualifier, see `self_form`.
                            nw.repo = None;
                            nw
                        };
                        let mut into_edits = vec![(fold_span, fold_text)];
                        into_edits.extend(crate::rename::ref_edits(
                            &into_content,
                            &into_other,
                            &local,
                        )?);
                        let into_after = crate::rename::apply_edits(&into_content, into_edits)?;
                        // Cross-file referrers become `[[host^id]]`, the repo qualifier
                        // and fragments preserved, the block-id mapped the same way.
                        let cross_form = |w: &au_references::WikilinkRef| {
                            let mut nw = w.clone();
                            nw.target = into_stem.clone();
                            nw.block_id = map_bid(&w.block_id);
                            nw.anchor = None;
                            nw
                        };
                        for (source, spans) in &cross {
                            let rc = read_referrer_checked(
                                source,
                                cross_hashes.get(source).copied().flatten(),
                            )?;
                            let rewritten = crate::rename::rewrite_refs(&rc, spans, &cross_form)?;
                            // A referrer whose every reference is commit-pinned
                            // yields no edits, so the rewrite is byte-identical.
                            // Writing it anyway costs a disk write and wakes the
                            // watcher for a file that did not change.
                            if rewritten != rc {
                                std::fs::write(source, rewritten).map_err(|e| {
                                    crate::mutate::MutationReject::new(format!(
                                        "cannot write referrer {}: {e}",
                                        source.display()
                                    ))
                                })?;
                            }
                        }
                        // Write the host with the folded record, then delete the file.
                        let written = crate::mutate::write_file(&blocking_into, &into_after, None)?;
                        std::fs::remove_file(&blocking_file).map_err(|e| {
                            crate::mutate::MutationReject::new(format!(
                                "cannot delete {}: {e}",
                                blocking_file.display()
                            ))
                        })?;
                        // Fold the stamp into the host file the content was pulled into.
                        Ok(fold_write_stamps(&blocking_into, &stamps, written)?)
                    })?;
                blocking_handle.rebuild_paths(saga_dirty_set(&plan));
                Ok::<_, crate::mutate::MutationReject>((written, commits))
            })
            .await
            .expect("mutation task never panics");
            match outcome {
                Err(reject) => mutation_reject_frame(reject),
                Ok((written, commits)) => mutation_response(handle, &into, Some(written), commits),
            }
        }
        Mutation::RenameBlockId(args) => {
            let host = match resolve_mutation_target(handle, &args.path) {
                Ok(t) => t,
                Err(frame) => return frame,
            };
            if args.block_id == args.to_block_id {
                return mutation_reject_frame(crate::mutate::MutationReject::new(
                    "`block_id` and `to_block_id` are the same — nothing to rename",
                ));
            }
            if !au_parser::is_valid_block_id(&args.to_block_id) {
                return mutation_reject_frame(crate::mutate::MutationReject::new(format!(
                    "`to_block_id` {:?} is not a valid block-id — ids are [A-Za-z0-9_-]+",
                    args.to_block_id
                )));
            }
            // Over the held knowledge base: the host's repo membership, the declaration
            // span of `block_id` (the inline record's `^:` value), a check that
            // `to_block_id` is free in the host's file-local id namespace, and the
            // inbound referrers filtered to this block-id (cross-file
            // `[[host^id]]` and host-local `[[^id]]`).
            struct Located {
                decl_span: au_diagnostics::ByteRange,
                host_hash: Option<crate::ir::ContentHash>,
                referrers: BTreeMap<PathBuf, Vec<au_diagnostics::ByteRange>>,
                /// The held content hash of each referrer, for the read-before-write
                /// drift check when the saga re-reads it.
                referrer_hashes: BTreeMap<PathBuf, Option<crate::ir::ContentHash>>,
            }
            let gathered = handle.read(|v| {
                if v.repos.repo_of(&host).is_none() {
                    return Err("path escapes the workspace".to_string());
                }
                let entry = v
                    .catalog
                    .get(&host)
                    .ok_or_else(|| "the host file is not in the knowledge base".to_string())?;
                let inst =
                    match entry.parse.as_ref() {
                        FileParse::Instance {
                            instance: Some(inst),
                            ..
                        } => inst,
                        _ => return Err(
                            "the host file is not a typed instance — no inline record to rename"
                                .to_string(),
                        ),
                    };
                let targets = crate::resolution_build::record_targets_of_kb(v, &host, inst);
                let decl_span = targets.get(&args.block_id).map(|t| t.span).ok_or_else(|| {
                    format!(
                        "no inline record carries block-id `{}` in {}; block-id rename covers \
                         inline `^:` records only",
                        args.block_id,
                        host.display()
                    )
                })?;
                if host_block_ids(entry.parse.as_ref(), &targets).contains(&args.to_block_id) {
                    return Err(format!(
                        "block-id `{}` already exists in {} — pick a free id",
                        args.to_block_id,
                        host.display()
                    ));
                }
                let mut referrers: BTreeMap<PathBuf, Vec<au_diagnostics::ByteRange>> =
                    BTreeMap::new();
                let mut referrer_hashes: BTreeMap<PathBuf, Option<crate::ir::ContentHash>> =
                    BTreeMap::new();
                for bl in v.backlinks(&host) {
                    if bl.block_id.as_ref().map(|b| b.id.as_str()) == Some(args.block_id.as_str()) {
                        referrers
                            .entry(bl.source.clone())
                            .or_default()
                            .push(bl.span);
                        referrer_hashes
                            .entry(bl.source.clone())
                            .or_insert_with(|| v.catalog.get(&bl.source).and_then(|e| e.hash));
                    }
                }
                Ok(Located {
                    decl_span,
                    host_hash: entry.hash,
                    referrers,
                    referrer_hashes,
                })
            });
            let Located {
                decl_span,
                host_hash,
                referrers,
                referrer_hashes,
            } = match gathered {
                crate::Read::Ready { value: Ok(g), .. } => g,
                crate::Read::Ready {
                    value: Err(msg), ..
                } => {
                    return mutation_reject_frame(crate::mutate::MutationReject::new(msg));
                }
                crate::Read::NotReady => {
                    return serde_json::to_value(Response::not_ready())
                        .expect("response serializes");
                }
            };
            let mut touched = vec![host.clone()];
            touched.extend(referrers.keys().filter(|p| **p != host).cloned());
            let not_written = referrers_not_written(&referrers);
            // A referrer this block-id rename would rewrite that lives in a consumed
            // member rejects the whole refactor (GAP 2).
            if let Some(reject) =
                reject_if_consumed_referrers(handle, &written_referrers(&referrers, &not_written))
            {
                return mutation_reject_frame(reject);
            }
            let mut plan = resolve_saga_plan(handle, &touched);
            plan.exempt_from_clean_check = not_written;
            let marker_dir = saga_marker_dir(handle.root());
            let summary = format!(
                "rename block-id {} -> {} in {}",
                args.block_id, args.to_block_id, args.path
            );
            let blocking_handle = handle.clone();
            let blocking_host = host.clone();
            let stamps = args.stamps;
            let to_block_id = args.to_block_id.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                // A referrer keeps its target, anchor, `:field`, and `::repo`;
                // only the block-id swaps to the new id. A local-form `[[^id]]`
                // has an empty target, so it re-serializes as `[[^newId]]`.
                let transform = |w: &au_references::WikilinkRef| {
                    let mut nw = w.clone();
                    // Swap the id, preserve the referrer's own mode: a `^^id`
                    // block-referent stays block-referent, a bare `^id` stays
                    // navigational.
                    let referent = w.block_id.as_ref().is_some_and(|b| b.referent);
                    nw.block_id = Some(au_references::BlockId {
                        id: to_block_id.clone(),
                        referent,
                    });
                    nw
                };
                let (written, commits) =
                    apply_saga(&ShellGit, &marker_dir, &plan, summary, true, || {
                        let content = std::fs::read_to_string(&blocking_host).map_err(|e| {
                            crate::mutate::MutationReject::new(format!(
                                "cannot read {}: {e}",
                                blocking_host.display()
                            ))
                        })?;
                        if host_hash != Some(crate::ir::ContentHash::of(content.as_bytes())) {
                            return Err(crate::mutate::MutationReject::new(
                            "the file changed since the engine last read it — wait for the rebuild \
                             and retry",
                        ));
                        }
                        // The declaration (the `^:` value) plus any host-local
                        // referrers, merged into one last-first pass so neither shifts
                        // the other, whatever their order in the file.
                        let mut host_edits = vec![(decl_span, to_block_id.clone())];
                        if let Some(spans) = referrers.get(&blocking_host) {
                            host_edits
                                .extend(crate::rename::ref_edits(&content, spans, &transform)?);
                        }
                        let host_after = crate::rename::apply_edits(&content, host_edits)?;
                        // Every other referrer, rewritten in place.
                        for (source, spans) in &referrers {
                            if *source == blocking_host {
                                continue;
                            }
                            let rc = read_referrer_checked(
                                source,
                                referrer_hashes.get(source).copied().flatten(),
                            )?;
                            let rewritten = crate::rename::rewrite_refs(&rc, spans, &transform)?;
                            // A referrer whose every reference is commit-pinned
                            // yields no edits, so the rewrite is byte-identical.
                            // Writing it anyway costs a disk write and wakes the
                            // watcher for a file that did not change.
                            if rewritten != rc {
                                std::fs::write(source, rewritten).map_err(|e| {
                                    crate::mutate::MutationReject::new(format!(
                                        "cannot write referrer {}: {e}",
                                        source.display()
                                    ))
                                })?;
                            }
                        }
                        let written = crate::mutate::write_file(&blocking_host, &host_after, None)?;
                        // Fold the stamp into the host file carrying the record.
                        Ok(fold_write_stamps(&blocking_host, &stamps, written)?)
                    })?;
                blocking_handle.rebuild_paths(saga_dirty_set(&plan));
                Ok::<_, crate::mutate::MutationReject>((written, commits))
            })
            .await
            .expect("mutation task never panics");
            match outcome {
                Err(reject) => mutation_reject_frame(reject),
                Ok((written, commits)) => mutation_response(handle, &host, Some(written), commits),
            }
        }
        Mutation::SetIgnores(args) => set_ignores(handle, args).await,
        Mutation::SetConfig(args) => set_config(handle, args).await,
        Mutation::Register(args) => register(handle, args).await,
    }
}

/// The `register` config mutation: write or update one `{ name, remote, path }`
/// entry in the per-user registry (`repos.yaml`), through the governed channel.
///
/// A DEVICE-GLOBAL write, the file sits outside every repo, so there is NO git
/// commit, unlike `set_ignores`. The registry path comes from the engine's
/// injected device root (a test tempdir) or the real `$HOME/.arsumbris`; absent
/// when `$HOME` is unset, which rejects (nowhere to write). The
/// identity checks live in `register_entry`: a disagreeing remote, or a path
/// whose `repo.yaml` declares another name, rejects as
/// `dependency-identity-conflict`, nothing written. On success the registry
/// changed, so resolution changed: a full rebuild mounts a newly-registered
/// member before the response carries fresh state.
async fn register(handle: &EngineHandle, args: RegisterArgs) -> serde_json::Value {
    // The caller `handle_mutation` already holds the write lock; re-taking it here
    // would deadlock. The registry path is device-scoped, injected for tests.
    let Some(registry_path) = crate::repo::user_registry_path(handle.config()) else {
        return mutation_reject_frame(crate::mutate::MutationReject::new(
            "no device root (set $HOME) to register into".to_string(),
        ));
    };
    let name = crate::repo::RepoName(args.name.clone());
    let path = abs_arg(handle.root(), &args.path);
    let path_str = path.display().to_string();
    let remote = args.remote.clone();

    // The write plus rebuild are blocking (std::fs + a full build), off the
    // runtime workers like the other config mutations.
    let blocking_handle = handle.clone();
    let outcome =
        tokio::task::spawn_blocking(move || -> std::io::Result<crate::repo::RegisterOutcome> {
            let res = crate::repo::register_entry(&registry_path, &name, remote.as_deref(), &path)?;
            if matches!(res, crate::repo::RegisterOutcome::Written) {
                blocking_handle.rebuild();
            }
            Ok(res)
        })
        .await
        .expect("register task never panics");

    match outcome {
        Err(e) => mutation_reject_frame(crate::mutate::MutationReject::new(format!(
            "register write failed: {e}"
        ))),
        Ok(crate::repo::RegisterOutcome::Conflict(msg)) => {
            mutation_reject_frame(crate::mutate::MutationReject::new(msg))
        }
        Ok(crate::repo::RegisterOutcome::Written) => serde_json::json!({
            "type": "registered",
            "schema_version": SCHEMA_VERSION,
            "name": args.name,
            "path": path_str,
            "version": handle.version(),
        }),
    }
}

/// How `set_config` produces the bytes to write: a whole file, or a keyed edit.
/// Built once from the args (exactly one form), then applied against the CURRENT
/// file content inside the write's critical section.
enum ConfigWrite {
    Content(String),
    Edit {
        field_path: Vec<au_core::PathSegment>,
        patch: serde_json::Map<String, serde_json::Value>,
    },
}

/// The `set_config` mutation: write one consumer config file under the scoped
/// channel, a whole `content` or a keyed `edit`. Path-safety (and the reserved
/// `au-engine` owner segment) gate the two caller-controlled segments up front,
/// independent of scope. Repo scope commits per mutation through the saga; machine
/// scope writes a device-global file with no commit.
async fn set_config(handle: &EngineHandle, args: SetConfigArgs) -> serde_json::Value {
    // The caller `handle_mutation` already holds the write lock and gated on
    // readiness. Path-safety is checked at the verb, before any scope resolution.
    if let Err(e) = crate::repo::check_config_segments(&args.consumer, &args.file) {
        return mutation_reject_frame(crate::mutate::MutationReject::new(e));
    }
    // Exactly one of `content` / `edit`.
    let write = match (args.content, args.edit) {
        (Some(content), None) => ConfigWrite::Content(content),
        (None, Some(edit)) => ConfigWrite::Edit {
            field_path: edit.field_path.into_iter().map(Into::into).collect(),
            patch: edit.patch,
        },
        (Some(_), Some(_)) => {
            return mutation_reject_frame(crate::mutate::MutationReject::new(
                "set_config takes `content` OR `edit`, not both",
            ))
        }
        (None, None) => {
            return mutation_reject_frame(crate::mutate::MutationReject::new(
                "set_config needs `content` or `edit`",
            ))
        }
    };
    let ctx = SetConfigCtx {
        consumer: args.consumer,
        file: args.file,
        type_name: args.type_name,
        root: args.root,
        write,
        expected_hash: args.expected_hash,
    };
    match args.scope {
        ConfigScope::Repo => set_config_repo(handle, ctx).await,
        ConfigScope::Machine => set_config_machine(handle, ctx).await,
    }
}

/// The scope-independent inputs a `set_config` write carries past the args parse.
struct SetConfigCtx {
    consumer: String,
    file: String,
    type_name: String,
    root: Option<String>,
    write: ConfigWrite,
    expected_hash: Option<String>,
}

/// Produce the content `set_config` writes, from the CURRENT on-disk content
/// (`None` when absent). The whole-`content` form injects the declared `type:`
/// when the content does not already self-describe; the `edit` form injects it
/// into the current file first (so the splice parses a typed record and the file
/// self-describes), then splices one keyed record, comments and siblings
/// preserved. The splice's own structural check rejects a change that would break
/// the YAML.
fn produce_config_content(
    current: Option<&str>,
    write: &ConfigWrite,
    type_name: &str,
    path: &Path,
) -> Result<String, crate::mutate::MutationReject> {
    let inject = |c: &str| {
        if crate::parse::content_declares_type(c) {
            c.to_string()
        } else {
            format!("type: {type_name}\n{c}")
        }
    };
    match write {
        ConfigWrite::Content(content) => Ok(inject(content)),
        ConfigWrite::Edit { field_path, patch } => {
            let current = current.ok_or_else(|| {
                crate::mutate::MutationReject::new(
                    "cannot `edit` an absent config file; create it first with `content`",
                )
            })?;
            let based = inject(current);
            crate::mutate::splice_edit_record(&based, path, field_path, patch)
        }
    }
}

/// The machine-scope `set_config` write: a device-global file under
/// `~/.arsumbris/<consumer>/config/<file>`, resolved from the engine's
/// `ConfigSource` (a test tempdir, the real `$HOME/.arsumbris`, or nothing). NO
/// git commit (the file sits outside every repo) and NO rebuild (it is not a
/// graph node), the shape `register` uses. A cross-daemon file lock serializes
/// concurrent writers of the one device-global file, and the produce-write is one
/// critical section under it so a keyed `edit`'s read + splice + write stays
/// atomic. `expected_hash` is a compare-and-set under the lock.
async fn set_config_machine(handle: &EngineHandle, ctx: SetConfigCtx) -> serde_json::Value {
    let Some(base) = handle.config().base() else {
        return mutation_reject_frame(crate::mutate::MutationReject::new(
            "no device root (set $HOME) to write machine-scope config into",
        ));
    };
    let path = match crate::repo::config_path(&base, &ctx.consumer, &ctx.file) {
        Ok(p) => p,
        Err(e) => return mutation_reject_frame(crate::mutate::MutationReject::new(e)),
    };
    let write_path = path.clone();
    let SetConfigCtx {
        type_name,
        write,
        expected_hash,
        ..
    } = ctx;
    let outcome = tokio::task::spawn_blocking(move || {
        crate::repo::write_device_config_with(&write_path, expected_hash.as_deref(), |current| {
            produce_config_content(current, &write, &type_name, &write_path)
        })
    })
    .await
    .expect("mutation task never panics");

    match outcome {
        Err(reject) => mutation_reject_frame(reject),
        // No commit and no catalog entry, so an EMPTY commits set (`commit` is
        // null off-repo) and `written_hash` None (out-of-band, `reflected` true),
        // with the written hash surfaced via `extra`. The uniform mutation frame,
        // minus the commit the repo scope carries.
        Ok(hash) => {
            let mut extra = serde_json::Map::new();
            extra.insert(
                "hash".into(),
                serde_json::Value::String(crate::mutate::hash_hex(hash)),
            );
            mutation_response_extra(handle, &path, None, Vec::new(), extra)
        }
    }
}

/// The repo-scope `set_config` write: construct `<member>/.arsumbris/<consumer>/
/// config/<file>` directly (a guard bypass, like `set_ignores`), inject `type:` so
/// the file self-describes, `expected_hash` compare-and-set, then commit per
/// mutation through the config-mutation saga. The file rides the `.arsumbris`
/// floor, so it is out-of-band: no catalog entry, no re-scope, no rebuild — the
/// graph is unaffected, so the held version does not move.
async fn set_config_repo(handle: &EngineHandle, ctx: SetConfigCtx) -> serde_json::Value {
    // Resolve the member root: an explicit `root` (a declared member), else the
    // served entry. The type is validated only where it resolves; the write itself
    // is store-and-advisory, never refused on a shape verdict.
    let want_root = ctx.root.as_deref().map(|r| abs_arg(handle.root(), r));
    // Resolve the owning member AND gate it on editability: repo-scope config is a
    // committed write into the member's `.arsumbris/`, so a consumed (`discover` /
    // `dep`) member is not the engine's to author, exactly like any other write.
    // `Ok(None)` names no member, `Err` is the not-editable reject.
    let resolved = handle.read(move |v| {
        let m = match &want_root {
            Some(p) => v.repos.repos().iter().find(|r| &r.root == p),
            None => v.repos.root(),
        };
        match m {
            None => Ok(None),
            Some(r) => {
                let role = crate::wire::member_role(v, &r.name);
                if role.editable() {
                    Ok(Some(r.root.clone()))
                } else {
                    Err(not_editable_reject(&r.name, role))
                }
            }
        }
    });
    let member_root = match resolved.value() {
        None => return serde_json::to_value(Response::not_ready()).expect("response serializes"),
        Some(Err(reject)) => return mutation_reject_frame(reject),
        Some(Ok(None)) => {
            return mutation_reject_frame(crate::mutate::MutationReject::new(format!(
                "repo scope needs a declared member root; `{}` names none",
                ctx.root.as_deref().unwrap_or("<entry>")
            )))
        }
        Some(Ok(Some(root))) => root,
    };

    let path =
        match crate::repo::config_path(&member_root.join(".arsumbris"), &ctx.consumer, &ctx.file) {
            Ok(p) => p,
            Err(e) => return mutation_reject_frame(crate::mutate::MutationReject::new(e)),
        };

    let plan = resolve_saga_plan(handle, &[path.clone()]);
    let marker_dir = saga_marker_dir(handle.root());
    let summary = format!("set_config {}/{}", ctx.consumer, ctx.file);
    let write_path = path.clone();
    let SetConfigCtx {
        type_name,
        write,
        expected_hash,
        ..
    } = ctx;

    let outcome = tokio::task::spawn_blocking(move || {
        // The read + produce + write is inside the saga's write closure (the write
        // lock is held), so a keyed `edit`'s splice sees the current bytes and the
        // `expected_hash` compare-and-set guards them. No rebuild: the file is
        // floored, so no graph state changed.
        apply_saga(&ShellGit, &marker_dir, &plan, summary, false, || {
            let current = std::fs::read_to_string(&write_path).ok();
            let new = produce_config_content(current.as_deref(), &write, &type_name, &write_path)?;
            crate::mutate::write_file(&write_path, &new, expected_hash.as_deref())
        })
    })
    .await
    .expect("mutation task never panics");

    match outcome {
        Err(reject) => mutation_reject_frame(reject),
        Ok((hash, commits)) => {
            // The file is out-of-band (not catalogued), so `written_hash` is None
            // — nothing to lag, `reflected` stays true, like `set_ignores`. The
            // written hash is surfaced explicitly so the consumer's next
            // `expected_hash` compare-and-set has it.
            let mut extra = serde_json::Map::new();
            extra.insert(
                "hash".into(),
                serde_json::Value::String(crate::mutate::hash_hex(hash)),
            );
            mutation_response_extra(handle, &path, None, commits, extra)
        }
    }
}

/// The `set_ignores` config mutation: replace a member's `.auignore` through the
/// governed channel. Distinct from the graph mutations above — it edits the
/// scope config that shapes which content enters the graph, not graph content,
/// so it MAY orphan references (advisory), and the strict no-dangling contract is
/// relaxed for it deliberately.
///
/// The `.arsumbris/` write-guard on the generic verbs stays absolute: this verb
/// does not route through the guarded resolver, it constructs the SINGLE allowed
/// path `<root>/.arsumbris/.auignore` from a validated member root and writes
/// that directly. So the exception is one tightly-scoped path, never a hole in
/// the guard.
///
/// The sequence is the mutation-channel shape: validate the patterns up front (a
/// malformed pattern rejects with the reason, nothing written), write (or, for
/// an empty list, remove) the file, commit per mutation via the saga (a shared
/// `Mutation-Id` trailer, like any governed write), then re-scope — the
/// `.arsumbris/` change forces a full re-walk (already wired) — and respond with
/// fresh state.
async fn set_ignores(handle: &EngineHandle, args: SetIgnoresArgs) -> serde_json::Value {
    // The caller `handle_mutation` already holds the write lock and gated on
    // readiness; re-taking the lock here would deadlock (it is not re-entrant).
    // `root` must name a declared member root exactly (the `ignores` read
    // surfaces it). A subdir or a path under no member rejects: the `.auignore`
    // is per-member-root.
    let member_root = abs_arg(handle.root(), &args.root);
    // Resolve the owning member AND gate on editability: `.auignore` is a committed
    // write into the member's `.arsumbris/`, so a consumed (`discover` / `dep`)
    // member is not the engine's to author. `None` names no member.
    let member = handle
        .read(|v| {
            v.repos
                .repos()
                .iter()
                .find(|r| r.root == member_root)
                .map(|r| (r.name.clone(), crate::wire::member_role(v, &r.name)))
        })
        .value()
        .flatten();
    let Some((member_name, role)) = member else {
        return mutation_reject_frame(crate::mutate::MutationReject::new(format!(
            "root must name a declared member root (the `ignores` read surfaces it): {}",
            args.root
        )));
    };
    if !role.editable() {
        return mutation_reject_frame(not_editable_reject(&member_name, role));
    }

    // Validate the patterns before touching disk: build the same `WalkFilter` the
    // build uses; a malformed pattern rejects with the reason, nothing written.
    // This is the win of the governed path over a raw file write, which would
    // degrade the same pattern to the `auignore-load-error` advisory.
    let joined = args.patterns.join("\n");
    if let Err(e) = au_parser::WalkFilter::with_auignore(&member_root, &joined) {
        return mutation_reject_frame(crate::mutate::MutationReject::new(format!(
            "malformed .auignore pattern, nothing written: {e}"
        )));
    }

    let auignore = member_root.join(".arsumbris").join(".auignore");
    let remove = args.patterns.is_empty();
    let plan = resolve_saga_plan(handle, &[auignore.clone()]);
    let marker_dir = saga_marker_dir(handle.root());
    let summary = format!("set_ignores {}", args.root);

    let blocking_handle = handle.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let ((), commits) = apply_saga(&ShellGit, &marker_dir, &plan, summary, false, || {
            if remove {
                // Empty list: remove the file, reverting to the default
                // excludes. An already-absent file is a clean no-op (the saga
                // sees no diff and makes no commit), never the reject
                // `delete_file` gives for a missing target.
                match std::fs::remove_file(&auignore) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(crate::mutate::MutationReject::new(format!(
                        "cannot remove {}: {e}",
                        auignore.display()
                    ))),
                }
            } else {
                // One pattern per line, trailing newline, so the file reads
                // back as clean lines.
                let content = format!("{joined}\n");
                crate::mutate::write_file(&auignore, &content, None).map(|_| ())
            }
        })?;
        blocking_handle.rebuild_paths(saga_dirty_set(&plan));
        Ok::<_, crate::mutate::MutationReject>(commits)
    })
    .await
    .expect("mutation task never panics");

    match outcome {
        Err(reject) => mutation_reject_frame(reject),
        // The `.auignore` is out-of-band config, never a catalog entry, so there
        // is no content hash to lag: `reflected` is the delete-style "nothing to
        // lag", and the re-scope ran synchronously in the blocking task before
        // this response, so the held version already reflects the new scope.
        Ok(commits) => {
            let response_path = member_root.join(".arsumbris").join(".auignore");
            mutation_response(handle, &response_path, None, commits)
        }
    }
}

/// One dependency member's resolve outcome, success or failure.
struct ResolveMemberSummary {
    name: String,
    sha: Option<String>,
    remote: Option<String>,
    path: Option<String>,
    error: Option<String>,
}

/// The whole resolve's outcome, computed off the runtime workers.
struct ResolveSummary {
    members: Vec<ResolveMemberSummary>,
    /// Each editable repo whose lock committed this resolve, its name to the lock
    /// commit sha. Empty when nothing resolved, no repo is a git tree, or every
    /// lock was unchanged (the idempotent no-op). One `Mutation-Id` correlates the
    /// whole set. Mirrors the mutation frame's `commits: { [repo]: sha }`.
    commits: BTreeMap<String, String>,
    /// Each repo whose lock commit failed, its name to the git error. A set entry
    /// means that repo's lock is on disk but uncommitted, soft-inconsistent,
    /// distinct from an unchanged lock (which is absent, a no-op).
    commit_errors: BTreeMap<String, String>,
    /// One message per package required at conflicting versions across the
    /// closure. A conflicted package resolves to no single version, so it is
    /// excluded from the lock and left unmounted; the engine never picks a winner.
    conflicts: Vec<String>,
}

/// The nearest ancestor directory of `path` that is a git working tree.
///
/// The lock commits into the git tree that physically holds it, not the au-repo
/// the catalog maps the manifest to: the workspace dir is the user's project repo
/// whether or not it declares a `repo.yaml`.
fn enclosing_git_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Run a resolve: fetch the served workspace's declared dependency closure into
/// the device cache, write each editable repo's own `repo.lock`, commit each into
/// the git tree that holds it, and rebuild so the mounted knowledge base reflects the result.
///
/// A write-path verb, not a `read`: it writes per-repo engine-schema locks and
/// rebuilds. The resolver is per-repo independent, so a
/// dependency that fails to resolve is reported in `failed` while the rest still
/// lock and mount. Each lock records only what resolved, each snapshot atomic, so
/// it never references a half-fetched package. The per-repo lock commits share ONE
/// `Mutation-Id`, so the whole solve is one auditable, correlated set. A version
/// conflict is reported in the frame's `conflicts`, not in the rebuilt knowledge base: the
/// written locks exclude conflicts, so the rebuild's locate re-detects nothing.
async fn handle_resolve(handle: &EngineHandle) -> serde_json::Value {
    // The resolve write shares the mutation serialization lock, so it never
    // interleaves with a saga or another resolve.
    let _write = handle.write_lock().lock().await;
    if handle.version().is_none() {
        return serde_json::to_value(Response::not_ready()).expect("response serializes");
    }

    let registry = crate::repo::load_user_registry(handle.config(), &au_parser::RealFileSystem);
    // Resolve the assembled workspace's members (a `workspace.yaml` composition,
    // else the entry repo plus its `deps`), each editable member writing its own
    // `.arsumbris/repo.lock` and the entry its `workspace.lock`.
    let manifest: Option<std::path::PathBuf> =
        crate::build::assembly_roots(handle.entry(), &registry, &au_parser::RealFileSystem).1;
    let cache_root = match handle.package_cache_root() {
        Some(r) => r.to_path_buf(),
        None => {
            return error_frame(
                "no package cache root; the home directory is unknown".to_string(),
                None,
                "resolve",
            )
        }
    };
    let registry_remote = handle.registry_remote().to_string();
    let root = handle.root().to_path_buf();

    // The fetch, lock write, commit, and rebuild are blocking; keep them off the
    // runtime workers, like a mutation write.
    let blocking_handle = handle.clone();
    let outcome = tokio::task::spawn_blocking(move || -> std::io::Result<ResolveSummary> {
        // The SAME local walk-resolve fixpoint `build` runs, so resolve sees exactly
        // the members `build` reads (a member nested under a declared member
        // included) and fetches / locks their deps. Cache-free; `resolve_and_lock`
        // fetches the closure below.
        let entry_ws_file = match &manifest {
            Some(m) => Some((m.clone(), std::fs::read(m)?)),
            None => None,
        };
        let mut ws = crate::build::local_fixpoint(
            &root,
            &entry_ws_file,
            &registry,
            &au_parser::RealFileSystem,
        )
        .ws;
        let cache = crate::pkgcache::PackageCache::new(cache_root);
        let (resolutions, conflict_diags, written) =
            crate::pkgcache::resolve_and_lock(&mut ws, &cache, &registry_remote)?;
        let conflicts: Vec<String> = conflict_diags.iter().map(|d| d.message.clone()).collect();

        let members: Vec<ResolveMemberSummary> = resolutions
            .iter()
            .map(|r| {
                let (sha, error) = match &r.outcome {
                    Ok(p) => (Some(p.sha.clone()), None),
                    Err(e) => (None, Some(e.message.clone())),
                };
                ResolveMemberSummary {
                    name: r.name.as_str().to_string(),
                    sha,
                    remote: r.remote.clone(),
                    path: r.path.clone(),
                    error,
                }
            })
            .collect();

        // `resolve_and_lock` wrote one lock per editable repo whose closure
        // changed. Commit EACH into the git tree that holds it, correlated by ONE
        // shared `Mutation-Id`, so a partial failure is one auditable saga. Per
        // repo, three outcomes: a real commit, an idempotent no-op (the lock is
        // unchanged at HEAD), or a genuine git failure (the lock is on disk but
        // uncommitted). Then rebuild over every written lock.
        let id = crate::mutate::generate_mutation_id();
        let mut commits: BTreeMap<String, String> = BTreeMap::new();
        let mut commit_errors: BTreeMap<String, String> = BTreeMap::new();
        for (name, lock_path) in &written {
            if let Some(repo_root) = enclosing_git_root(lock_path) {
                match crate::pkgcache::commit_package_lock(&repo_root, lock_path, name.clone(), &id)
                {
                    Ok(Some(sha)) => {
                        commits.insert(name.as_str().to_string(), sha.0);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        commit_errors.insert(name.as_str().to_string(), e.message);
                    }
                }
            }
        }
        if !written.is_empty() {
            blocking_handle.rebuild_paths(written.iter().map(|(_, p)| p.clone()).collect());
        }

        Ok(ResolveSummary {
            members,
            commits,
            commit_errors,
            conflicts,
        })
    })
    .await
    .expect("resolve task never panics");

    let summary = match outcome {
        Ok(s) => s,
        Err(e) => return error_frame(format!("resolve failed: {e}"), None, "resolve"),
    };

    let resolved: Vec<_> = summary
        .members
        .iter()
        .filter(|m| m.error.is_none())
        .map(|m| {
            serde_json::json!({
                "name": m.name,
                "sha": m.sha,
                "remote": m.remote,
                "path": m.path,
            })
        })
        .collect();
    let failed: Vec<_> = summary
        .members
        .iter()
        .filter(|m| m.error.is_some())
        .map(|m| {
            serde_json::json!({
                "name": m.name,
                "reason": m.error,
                "code": crate::repo::DEPENDENCY_RESOLUTION_FAILED.as_str(),
            })
        })
        .collect();

    serde_json::json!({
        "type": "resolved",
        "schema_version": SCHEMA_VERSION,
        "resolved": resolved,
        "failed": failed,
        "conflicts": summary.conflicts,
        "commits": summary.commits,
        "commit_errors": summary.commit_errors,
        "version": handle.version(),
    })
}

/// `assign_block_id`: the engine assigns a `^:` id to the addressable
/// entity enclosing the byte offset and returns `{ id, ref }`. A YAML
/// inline record gains `^: <id>` as its first key; a markdown block gains
/// a trailing ` ^<id>` marker. Assigning to a record that already carries
/// an id is idempotent — the existing id comes back, nothing is written.
/// The held parse locates the record, so a disk file that drifted from the
/// catalog rejects (the offset would be unreliable).
/// `edit_record`: patch a nested record's fields in place, then run the shared
/// nested-record saga (read, CAS, splice, on_invalid gate, write, commit).
async fn handle_edit_record(handle: &EngineHandle, args: EditRecordArgs) -> serde_json::Value {
    let target = match resolve_mutation_target(handle, &args.path) {
        Ok(t) => t,
        Err(frame) => return frame,
    };
    let field_path: Vec<au_core::PathSegment> =
        args.field_path.into_iter().map(Into::into).collect();
    let patch = args.patch;
    let splice_target = target.clone();
    nested_record_saga(
        handle,
        target,
        format!("edit_record {}", args.path),
        args.expected_hash,
        args.on_invalid,
        args.stamps,
        move |content| {
            crate::mutate::splice_edit_record(content, &splice_target, &field_path, &patch)
        },
    )
    .await
}

/// `append_record`: add one element to a sequence field, through the shared
/// nested-record saga.
async fn handle_append_record(handle: &EngineHandle, args: AppendRecordArgs) -> serde_json::Value {
    let target = match resolve_mutation_target(handle, &args.path) {
        Ok(t) => t,
        Err(frame) => return frame,
    };
    let field_path: Vec<au_core::PathSegment> =
        args.field_path.into_iter().map(Into::into).collect();
    let value = args.value;
    let splice_target = target.clone();
    nested_record_saga(
        handle,
        target,
        format!("append_record {}", args.path),
        args.expected_hash,
        args.on_invalid,
        args.stamps,
        move |content| {
            crate::mutate::splice_append_record(content, &splice_target, &field_path, &value)
        },
    )
    .await
}

/// The shared execution core for the nested-record mutations. Under the write
/// lock, in one blocking task: read the current bytes (the CAS input and the
/// splice input), guard `expected_hash`, `splice` to the candidate content, run
/// the `on_invalid` gate, write, commit as a single-file saga, rebuild, respond.
///
/// The splice itself is a pure transform that self-checks structural validity,
/// so a splice that would break the YAML rejects before any write, upholding the
/// never-broken invariant even under `on_invalid: advise`.
async fn nested_record_saga(
    handle: &EngineHandle,
    target: PathBuf,
    summary: String,
    expected_hash: Option<String>,
    on_invalid: OnInvalid,
    stamps: Vec<Stamp>,
    splice: impl FnOnce(&str) -> Result<String, crate::mutate::MutationReject> + Send + 'static,
) -> serde_json::Value {
    let plan = resolve_saga_plan(handle, &[target.clone()]);
    let marker_dir = saga_marker_dir(handle.root());
    let blocking_handle = handle.clone();
    let write_target = target.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let gate_handle = blocking_handle.clone();
        let write_path = write_target.clone();
        let (written, commits) =
            apply_saga(&ShellGit, &marker_dir, &plan, summary, false, move || {
                // The current on-disk bytes: the CAS reference and the splice input.
                let current = std::fs::read_to_string(&write_path).map_err(|e| {
                    crate::mutate::MutationReject::new(format!(
                        "cannot read {}: {e}",
                        write_path.display()
                    ))
                })?;
                if let Some(expected) = &expected_hash {
                    let current_hex =
                        crate::mutate::hash_hex(crate::ir::ContentHash::of(current.as_bytes()));
                    if &current_hex != expected {
                        return Err(crate::mutate::MutationReject {
                            message: format!(
                                "expected_hash mismatch: the file changed since it was read \
                                 (current {current_hex})"
                            ),
                            detail: Some(serde_json::json!({ "current_hash": current_hex })),
                        });
                    }
                }
                let candidate = splice(&current)?;
                if on_invalid == OnInvalid::Reject {
                    if let Read::Ready {
                        value: Err(reject), ..
                    } = gate_handle
                        .read(|v| reject_on_new_errors(v, &write_path, &current, &candidate))
                    {
                        return Err(reject);
                    }
                }
                let written = crate::ir::ContentHash::of(candidate.as_bytes());
                std::fs::write(&write_path, &candidate).map_err(|e| {
                    crate::mutate::MutationReject::new(format!(
                        "cannot write {}: {e}",
                        write_path.display()
                    ))
                })?;
                // Fold the stamps into the just-written file, sharing this commit.
                fold_write_stamps(&write_path, &stamps, written)
            })?;
        blocking_handle.rebuild_paths(saga_dirty_set(&plan));
        Ok::<_, crate::mutate::MutationReject>((written, commits))
    })
    .await
    .expect("mutation task never panics");
    match outcome {
        Err(reject) => mutation_reject_frame(reject),
        Ok((written, commits)) => mutation_response(handle, &target, Some(written), commits),
    }
}

/// The `on_invalid: reject` verdict: refuse when the candidate raises the file's
/// validation-ERROR count over the current content's. Both are measured by the
/// same in-memory validation, so the delta is meaningful across the byte shift.
/// A pre-existing error never blocks; only an increase does.
fn reject_on_new_errors(
    kb: &KnowledgeBase,
    path: &Path,
    current: &str,
    candidate: &str,
) -> Result<(), crate::mutate::MutationReject> {
    let before = crate::value_validate::instance_error_count(kb, path, current);
    let after_diags = crate::value_validate::validate_instance_content(kb, path, candidate);
    let after = after_diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    if after > before {
        let codes: Vec<&str> = after_diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| d.code.as_str())
            .collect();
        return Err(crate::mutate::MutationReject::new(format!(
            "on_invalid=reject: the edit adds validation errors ({}); nothing written",
            codes.join(", ")
        )));
    }
    Ok(())
}

async fn assign_block_id(handle: &EngineHandle, args: AssignBlockIdArgs) -> serde_json::Value {
    let target = match resolve_mutation_target(handle, &args.path) {
        Ok(t) => t,
        Err(frame) => return frame,
    };

    // Locate the enclosing entity in the held parse, coherent at a version.
    enum Plan {
        Existing(String),
        InsertRecord { record_start: usize },
        MarkdownMarker,
    }
    let located = handle.read(|v| {
        let entry = v.catalog.get(&target)?;
        let plan = match entry.parse.as_ref() {
            FileParse::Instance {
                instance: Some(inst),
                body,
                body_offset,
                is_markdown,
                ..
            } => {
                if let Some(hit) = record_at(&inst.fields, args.at) {
                    match hit.existing {
                        Some(id) => Plan::Existing(id),
                        None => Plan::InsertRecord {
                            record_start: hit.start,
                        },
                    }
                } else if *is_markdown && args.at >= *body_offset {
                    // Idempotent like the record path: a block that already
                    // carries a marker returns its id unwritten.
                    match crate::mutate::existing_markdown_marker(body, args.at - *body_offset) {
                        Some(id) => Plan::Existing(id),
                        None => Plan::MarkdownMarker,
                    }
                } else {
                    return None;
                }
            }
            FileParse::Note {
                body, body_offset, ..
            } => match args
                .at
                .checked_sub(*body_offset)
                .and_then(|rel| crate::mutate::existing_markdown_marker(body, rel))
            {
                Some(id) => Plan::Existing(id),
                None => Plan::MarkdownMarker,
            },
            _ => return None,
        };
        Some((plan, entry.hash))
    });
    let Read::Ready { value: located, .. } = located else {
        return serde_json::to_value(Response::not_ready()).expect("response serializes");
    };
    let Some((plan, catalog_hash)) = located else {
        return mutation_reject_frame(crate::mutate::MutationReject {
            message: "no addressable entity encloses the offset — pass an offset inside an \
                      inline record or a markdown block"
                .to_string(),
            detail: None,
        });
    };

    // Idempotent: an already-addressable record returns its id unwritten — UNLESS
    // a stamp rides along, which still lands its own commit even when the block-id
    // write is a no-op. Without any stamp, no saga, no commit.
    if let Plan::Existing(id) = &plan {
        if args.stamps.is_empty() {
            let mut extra = serde_json::Map::new();
            extra.insert("id".into(), serde_json::Value::String(id.clone()));
            extra.insert(
                "ref".into(),
                serde_json::Value::String(block_ref(handle, &target, id)),
            );
            return mutation_response_extra(handle, &target, None, Vec::new(), extra);
        }
    }

    let blocking_handle = handle.clone();
    let blocking_target = target.clone();
    let at = args.at;
    let stamps = args.stamps;
    let saga = resolve_saga_plan(handle, &[target.clone()]);
    let marker_dir = saga_marker_dir(handle.root());
    let summary = format!("assign_block_id {}", args.path);
    let outcome = tokio::task::spawn_blocking(move || {
        // assign_block_id adds a fresh `^:id` to ONE record in its host file. The
        // id is new, so there are no `[[host^id]]` referrers to cascade — a
        // single-file edit, not a referrer-rewriting refactor, so it keeps the
        // non-git write-without-commit fallback.
        let ((id, written), commits) =
            apply_saga(&ShellGit, &marker_dir, &saga, summary, false, || {
                let content = std::fs::read_to_string(&blocking_target).map_err(|e| {
                    crate::mutate::MutationReject {
                        message: format!("cannot read {}: {e}", blocking_target.display()),
                        detail: None,
                    }
                })?;
                // The offsets came from the held parse; a file that drifted on disk
                // makes them unreliable. Read-before-write, structurally.
                if catalog_hash != Some(crate::ir::ContentHash::of(content.as_bytes())) {
                    return Err(crate::mutate::MutationReject {
                        message: "the file changed since the engine last read it — wait for the \
                              rebuild and retry with a fresh offset"
                            .to_string(),
                        detail: None,
                    });
                }
                // Write the block-id (except in the idempotent-existing case, where
                // the id is already present and only the stamp lands).
                let write_updated = |updated: String| -> Result<
                    crate::ir::ContentHash,
                    crate::mutate::MutationReject,
                > {
                    let h = crate::ir::ContentHash::of(updated.as_bytes());
                    std::fs::write(&blocking_target, updated).map_err(|e| {
                        crate::mutate::MutationReject {
                            message: format!("cannot write {}: {e}", blocking_target.display()),
                            detail: None,
                        }
                    })?;
                    Ok(h)
                };
                let (id, written) = match plan {
                    Plan::Existing(id) => (id, crate::ir::ContentHash::of(content.as_bytes())),
                    Plan::InsertRecord { record_start } => {
                        let id = crate::mutate::generate_block_id(|c| content.contains(c));
                        let h = write_updated(crate::mutate::insert_record_id(
                            &content,
                            record_start,
                            &id,
                        )?)?;
                        (id, h)
                    }
                    Plan::MarkdownMarker => {
                        let id = crate::mutate::generate_block_id(|c| content.contains(c));
                        let h = write_updated(crate::mutate::insert_markdown_marker(
                            &content, at, &id,
                        )?)?;
                        (id, h)
                    }
                };
                // Fold the stamp into the file, sharing this commit.
                let written = fold_write_stamps(&blocking_target, &stamps, written)?;
                Ok((id, written))
            })?;
        blocking_handle.rebuild_paths(saga_dirty_set(&saga));
        Ok::<_, crate::mutate::MutationReject>((id, written, commits))
    })
    .await
    .expect("mutation task never panics");
    match outcome {
        Ok((id, written, commits)) => {
            let mut extra = serde_json::Map::new();
            extra.insert("id".into(), serde_json::Value::String(id.clone()));
            extra.insert(
                "ref".into(),
                serde_json::Value::String(block_ref(handle, &target, &id)),
            );
            mutation_response_extra(handle, &target, Some(written), commits, extra)
        }
        Err(reject) => mutation_reject_frame(reject),
    }
}

/// The innermost inline record enclosing a byte offset, with its existing
/// `^:` id if it carries one. `start` is the record's first-key offset, the
/// insertion point for a new id; `span` is the record's whole value extent, the
/// slice `promote` lifts out.
struct RecordHit {
    start: usize,
    span: au_diagnostics::ByteRange,
    existing: Option<String>,
}

fn record_at(fields: &[au_core::InstanceField], at: usize) -> Option<RecordHit> {
    fn walk(
        value: &au_core::InstanceValue,
        span: au_diagnostics::ByteRange,
        at: usize,
        best: &mut Option<RecordHit>,
    ) {
        if at < span.start || at >= span.end {
            return;
        }
        match value {
            au_core::InstanceValue::Mapping(inline) => {
                *best = Some(RecordHit {
                    start: span.start,
                    span,
                    existing: inline.block_id.as_ref().map(|b| b.id.clone()),
                });
                for f in &inline.fields {
                    walk(&f.value, f.value_span, at, best);
                }
            }
            au_core::InstanceValue::Sequence(elems) => {
                for e in elems {
                    walk(&e.value, e.span, at, best);
                }
            }
            _ => {}
        }
    }
    let mut best = None;
    for f in fields {
        walk(&f.value, f.value_span, at, &mut best);
    }
    best
}

/// The whole-value span of the innermost scalar value enclosing `point` — the
/// YAML value `inline` replaces with a folded record. A field value or a
/// sequence element, the value that holds the chosen `[[file]]` reference.
fn value_span_at(
    fields: &[au_core::InstanceField],
    point: usize,
) -> Option<au_diagnostics::ByteRange> {
    fn walk(
        value: &au_core::InstanceValue,
        span: au_diagnostics::ByteRange,
        point: usize,
    ) -> Option<au_diagnostics::ByteRange> {
        if point < span.start || point >= span.end {
            return None;
        }
        match value {
            au_core::InstanceValue::String(_) => Some(span),
            au_core::InstanceValue::Sequence(elems) => {
                elems.iter().find_map(|e| walk(&e.value, e.span, point))
            }
            au_core::InstanceValue::Mapping(inline) => inline
                .fields
                .iter()
                .find_map(|f| walk(&f.value, f.value_span, point)),
            _ => None,
        }
    }
    fields
        .iter()
        .find_map(|f| walk(&f.value, f.value_span, point))
}

/// Read a referrer slated for an in-place reference rewrite, rejecting if its
/// on-disk content drifted from the hash the engine held when the refactor was
/// planned. The spans being rewritten come from that held parse, so a drift
/// makes them unreliable — a displaced span could land on a different wikilink
/// and be silently mis-rewritten. clean-at-HEAD catches an uncommitted drift;
/// this catches a committed one the rebuild has not yet observed. Shared by every
/// reference-rewriting verb (rename / promote / inline / rename_block_id).
fn read_referrer_checked(
    path: &Path,
    held: Option<crate::ir::ContentHash>,
) -> Result<String, crate::mutate::MutationReject> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        crate::mutate::MutationReject::new(format!("cannot read referrer {}: {e}", path.display()))
    })?;
    if held != Some(crate::ir::ContentHash::of(content.as_bytes())) {
        return Err(crate::mutate::MutationReject::new(format!(
            "referrer {} changed since the engine last read it — wait for the rebuild and retry",
            path.display()
        )));
    }
    Ok(content)
}

/// Rewrite one `rename_type` referrer in place. Merges the precomputed type-name
/// edits with the wikilink edits derived now, applies them, then writes back.
fn rewrite_referrer(
    path: &Path,
    held: Option<crate::ir::ContentHash>,
    type_edits: Option<&Vec<(au_diagnostics::ByteRange, String)>>,
    link_spans: Option<&Vec<au_diagnostics::ByteRange>>,
    old_rel: &Path,
    new_rel: &Path,
) -> Result<(), crate::mutate::MutationReject> {
    let content = read_referrer_checked(path, held)?;
    let mut edits = type_edits.cloned().unwrap_or_default();
    if let Some(spans) = link_spans {
        edits.extend(crate::rename::link_edits(
            &content, spans, old_rel, new_rel,
        )?);
    }
    let rewritten = crate::rename::apply_edits(&content, edits)?;
    // A referrer whose every reference is commit-pinned yields no edits, so the
    // rewrite is byte-identical. Writing it anyway costs a disk write and wakes
    // the watcher for a file that did not change. The `rename_type` case can also
    // reach here with only TYPE-NAME edits, which do change the bytes, so this is
    // a byte comparison rather than an is-the-edit-list-empty test.
    if rewritten == content {
        return Ok(());
    }
    std::fs::write(path, rewritten).map_err(|e| {
        crate::mutate::MutationReject::new(format!("cannot write referrer {}: {e}", path.display()))
    })
}

/// Every block-id declared in a host file, across both addressable surfaces:
/// inline-record `^:` ids ([`au_core::collect_record_targets`], passed in) and
/// body markers (paragraph / heading / own-line markers and a fenced block's
/// trailing id). The file-local id namespace, used to reject a block-id rename
/// whose target id already exists on either surface.
fn host_block_ids(
    parse: &FileParse,
    record_targets: &au_core::RecordTargets,
) -> std::collections::BTreeSet<String> {
    let mut ids: std::collections::BTreeSet<String> = record_targets.keys().cloned().collect();
    if let Some((body, _)) = parse.markdown_body() {
        for ev in scan_body(body) {
            match ev {
                BodyEvent::BlockIdMarker { id, .. } => {
                    ids.insert(id.to_string());
                }
                BodyEvent::FencedBlock {
                    trailing_block_id: Some(id),
                    ..
                } => {
                    ids.insert(id.to_string());
                }
                _ => {}
            }
        }
    }
    ids
}

/// The inner wikilink text at `span` in a referrer's parse — a frontmatter
/// nav-link or a body wikilink. `None` if no wikilink occupies exactly that
/// span. Recovers the anchor the backlink index does not store, so the inline
/// referrer guard can reject a `[[file#head]]` referrer.
fn wikilink_inner_at(parse: &FileParse, span: au_diagnostics::ByteRange) -> Option<String> {
    fn from_value(
        value: &au_core::InstanceValue,
        nav_links: &[au_core::NavLink],
        span: au_diagnostics::ByteRange,
    ) -> Option<String> {
        for nl in nav_links {
            if nl.span == span {
                return Some(nl.raw.clone());
            }
        }
        match value {
            au_core::InstanceValue::Sequence(elems) => elems
                .iter()
                .find_map(|e| from_value(&e.value, &e.nav_links, span)),
            au_core::InstanceValue::Mapping(inline) => inline
                .fields
                .iter()
                .find_map(|f| from_value(&f.value, &f.nav_links, span)),
            _ => None,
        }
    }
    let fields = match parse {
        FileParse::Instance {
            instance: Some(inst),
            ..
        } => Some(&inst.fields),
        FileParse::Note { fields, .. } => Some(fields),
        _ => None,
    };
    if let Some(fields) = fields {
        if let Some(raw) = fields
            .iter()
            .find_map(|f| from_value(&f.value, &f.nav_links, span))
        {
            return Some(raw);
        }
    }
    if let Some((body, offset)) = parse.markdown_body() {
        for ev in scan_body(body) {
            if let BodyEvent::Wikilink { raw, span: s } = ev {
                if au_diagnostics::ByteRange::new(s.start + offset, s.end + offset) == span {
                    return Some(raw.to_string());
                }
            }
        }
    }
    None
}

/// The inline record carrying `^: <id>`, by its whole-value span. The stable
/// locator for a record that already has a block-id, complementing `record_at`'s
/// offset locator. First occurrence wins; a `block-id-duplicate` surfaces the
/// collision separately, so at most one record carries the id in a valid file.
fn record_by_block_id(fields: &[au_core::InstanceField], id: &str) -> Option<RecordHit> {
    fn walk(
        value: &au_core::InstanceValue,
        span: au_diagnostics::ByteRange,
        id: &str,
    ) -> Option<RecordHit> {
        match value {
            au_core::InstanceValue::Mapping(inline) => {
                if inline.block_id.as_ref().is_some_and(|b| b.id == id) {
                    return Some(RecordHit {
                        start: span.start,
                        span,
                        existing: Some(id.to_string()),
                    });
                }
                inline
                    .fields
                    .iter()
                    .find_map(|f| walk(&f.value, f.value_span, id))
            }
            au_core::InstanceValue::Sequence(elems) => {
                elems.iter().find_map(|e| walk(&e.value, e.span, id))
            }
            _ => None,
        }
    }
    fields.iter().find_map(|f| walk(&f.value, f.value_span, id))
}

/// The `[[target^id]]` ref for an assigned id: the bare stem when it
/// resolves uniquely back to this file, the repo-relative path otherwise.
fn block_ref(handle: &EngineHandle, target: &Path, id: &str) -> String {
    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let unique = handle
        .read(|v| {
            // Repo-local: the bare `[[stem^id]]` ref resolves within the
            // target file's own repo, so uniqueness is judged there.
            v.index_for_path(target)
                .resolve(&stem)
                .ok()
                .map(|p| p == target)
                .unwrap_or(false)
        })
        .value()
        .unwrap_or(false);
    if unique {
        return format!("[[{stem}^{id}]]");
    }
    let rel = target
        .strip_prefix(handle.root())
        .unwrap_or(target)
        .display()
        .to_string();
    format!("[[{rel}^{id}]]")
}

/// The success frame for a mutation with no primitive-specific fields, the
/// common case. See [`mutation_response_extra`].
/// The `preview_mutation` read: resolve the target path, then simulate the op
/// over an overlay and envelope its product. A path that mounts nowhere folds
/// into the result's `reject`, so a preview always answers as a read.
///
/// The simulate step runs on a blocking task: a preview does a scoped recompute
/// (or a whole-knowledge-base build for a type-def), the same shape a rebuild
/// runs, so it stays off the async reactor like a mutation's write does.
/// One trailer line a `commit_meta` record surfaces, `{ key, value }`,
/// uninterpreted. The engine's own (`Mutation-Id`, `Moved:`) and a caller
/// attribution line alike.
#[derive(Debug, Serialize)]
struct CommitTrailerDto {
    key: String,
    value: String,
}

/// One commit's metadata, the `commit_meta` read's per-commit record.
///
/// Positional against the request. An absent commit is `available: false` with
/// the metadata fields omitted, the shape of `pinned-commit-unavailable`.
#[derive(Debug, Serialize)]
struct CommitMetaDto {
    /// The resolved full oid when present, else the requested input verbatim.
    commit: String,
    available: bool,
    /// Committer date, unix seconds, when the commit entered history.
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<i64>,
    /// The author, `Name <email>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    /// The raw commit message, summary and body.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// Every trailer line, uninterpreted. Empty, never absent, so a consumer
    /// always reads an array.
    trailers: Vec<CommitTrailerDto>,
}

impl CommitMetaDto {
    /// An absent commit (unknown repo, non-git member, missing sha, or a git
    /// failure for its store), named rather than dropped.
    fn unavailable(commit: String) -> Self {
        Self {
            commit,
            available: false,
            timestamp: None,
            author: None,
            message: None,
            trailers: Vec::new(),
        }
    }

    fn from_record(r: &crate::gitwriter::CommitMetaRecord) -> Self {
        Self {
            commit: r.commit.clone(),
            available: r.available,
            timestamp: r.timestamp,
            author: r.author.clone(),
            message: r.message.clone(),
            trailers: r
                .trailers
                .iter()
                .map(|t| CommitTrailerDto {
                    key: t.key.clone(),
                    value: t.value.clone(),
                })
                .collect(),
        }
    }
}

/// The `commit_meta` read: resolve each requested commit to its member store,
/// then read metadata off git.
///
/// Two lock/reactor boundaries kept clean. The store resolution runs under the
/// state lock (in-memory, fast). The git reads run on a blocking task, off the
/// async reactor AND off the lock, since each spawns a subprocess. The result
/// is positional against the request, so an absent commit is `available: false`
/// rather than dropped.
async fn commit_meta(handle: &EngineHandle, commits: Vec<CommitRef>) -> Response {
    // A commit-ish is a single line. A newline would add extra `--batch-check`
    // probe lines and misalign the positional result, so reject it loudly rather
    // than silently return skewed records.
    if let Some(bad) = commits.iter().find(|c| c.commit.contains('\n')) {
        return Response::invalid(format!(
            "commit '{}' contains a newline; a commit-ish is a single line",
            bad.commit.escape_default()
        ));
    }
    let entry_root = handle.root().to_path_buf();
    let cache_root = handle.package_cache_root().map(Path::to_path_buf);
    // Under the lock: resolve each request to its git store (the covering
    // working tree). `repo` absent -> the entry repo; a named member -> its
    // covering tree, `None` when untracked (a .git-free snapshot) or unknown,
    // which the git pass renders `available: false`.
    let resolved = handle.read(move |v| {
        let members = wire::introspect_members(v, &entry_root, cache_root.as_deref());
        commits
            .into_iter()
            .map(|c| {
                let store = match &c.repo {
                    None => Some(entry_root.clone()),
                    Some(name) => members
                        .members
                        .iter()
                        .find(|m| &m.repo == name)
                        .and_then(|m| m.git.root.clone())
                        .map(PathBuf::from),
                };
                (c.commit, store)
            })
            .collect::<Vec<_>>()
    });
    let (version, requests) = match resolved {
        Read::NotReady => return Response::not_ready(),
        Read::Ready { version, value } => (version, value),
    };
    let dtos = tokio::task::spawn_blocking(move || commit_meta_git(requests))
        .await
        .expect("commit_meta task never panics");
    enveloped(
        "commit_meta",
        Read::Ready {
            version,
            value: dtos,
        },
    )
}

/// The git half of `commit_meta`, off the reactor. Groups the requests by store
/// so each store pays its two processes once (never one per commit), then
/// scatters the records back into request order.
fn commit_meta_git(requests: Vec<(String, Option<PathBuf>)>) -> Vec<CommitMetaDto> {
    let mut out: Vec<Option<CommitMetaDto>> = (0..requests.len()).map(|_| None).collect();
    let mut by_store: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    for (i, (_commit, store)) in requests.iter().enumerate() {
        match store {
            Some(root) => by_store.entry(root.clone()).or_default().push(i),
            None => out[i] = Some(CommitMetaDto::unavailable(requests[i].0.clone())),
        }
    }
    for (root, idxs) in by_store {
        let commits: Vec<String> = idxs.iter().map(|&i| requests[i].0.clone()).collect();
        // A git failure for a store degrades to unavailable for its commits, the
        // same as an absent sha, rather than failing the whole read.
        let recs = crate::gitwriter::ShellGit
            .commit_meta(&root, &commits)
            .unwrap_or_default();
        for (k, &i) in idxs.iter().enumerate() {
            out[i] = Some(match recs.get(k) {
                Some(rec) => CommitMetaDto::from_record(rec),
                None => CommitMetaDto::unavailable(requests[i].0.clone()),
            });
        }
    }
    out.into_iter()
        .map(|o| o.expect("every request index is filled"))
        .collect()
}

/// One commit in a file's history, the `file_history` read's per-row record.
///
/// A raw pass-through: `status` and `from` are git's own similarity heuristic,
/// best-effort, NOT authoritative. `message` is the SUBJECT line; the full body
/// is a `commit_meta` join away.
#[derive(Debug, Serialize)]
struct FileHistoryDto {
    commit: String,
    timestamp: i64,
    author: String,
    message: String,
    /// `added` / `modified` / `deleted` / `renamed`. `renamed` is git's
    /// heuristic, not authoritative.
    status: &'static str,
    /// The prior path on a `renamed`, git's heuristic match; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
}

impl FileHistoryDto {
    fn from_record(r: &crate::gitwriter::FileHistoryRecord) -> Self {
        Self {
            commit: r.commit.clone(),
            timestamp: r.timestamp,
            author: r.author.clone(),
            message: r.message.clone(),
            status: r.status,
            from: r.from.clone(),
        }
    }
}

/// The `file_history` read: resolve the file's absolute path against its member
/// root, then read its commit stream off git.
///
/// Same lock/reactor boundary as `commit_meta`: the path resolution runs under
/// the state lock (in-memory), the git read on a blocking task. Git resolves the
/// covering working tree from the file's own directory, so the routing is
/// member-correct with no tree-relative path math. An unknown `repo`, or a path
/// that is a repo-relative nowhere, yields an empty stream.
async fn file_history(handle: &EngineHandle, path: String, repo: Option<String>) -> Response {
    // `repo` names the root a RELATIVE path resolves against, so an absolute
    // path would make `repo` silently do nothing. Reject the contradiction
    // rather than reading a tree unrelated to the named member.
    if repo.is_some() && Path::new(&path).is_absolute() {
        return Response::invalid(
            "an absolute `path` cannot be combined with `repo`; `repo` names the root a relative `path` resolves against".to_string(),
        );
    }
    let entry_root = handle.root().to_path_buf();
    let cache_root = handle.package_cache_root().map(Path::to_path_buf);
    let resolved = handle.read(move |v| {
        // The base root the path is relative to: the entry, or the named
        // member's own root. An unknown member -> `None` -> empty stream.
        let base = match &repo {
            None => Some(entry_root.clone()),
            Some(name) => {
                let members = wire::introspect_members(v, &entry_root, cache_root.as_deref());
                members
                    .members
                    .iter()
                    .find(|m| &m.repo == name)
                    .map(|m| PathBuf::from(&m.root))
            }
        };
        base.map(|root| {
            let p = Path::new(&path);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            }
        })
    });
    let (version, abs) = match resolved {
        Read::NotReady => return Response::not_ready(),
        Read::Ready { version, value } => (version, value),
    };
    let rows = tokio::task::spawn_blocking(move || file_history_git(abs))
        .await
        .expect("file_history task never panics");
    enveloped(
        "file_history",
        Read::Ready {
            version,
            value: rows,
        },
    )
}

/// The git half of `file_history`, off the reactor. Runs git in the file's own
/// directory so git resolves the covering working tree.
fn file_history_git(abs: Option<PathBuf>) -> Vec<FileHistoryDto> {
    let Some(abs) = abs else {
        return Vec::new();
    };
    let (Some(dir), Some(name)) = (abs.parent(), abs.file_name()) else {
        return Vec::new();
    };
    crate::gitwriter::ShellGit
        .file_history(dir, name)
        .unwrap_or_default()
        .iter()
        .map(FileHistoryDto::from_record)
        .collect()
}

/// The default `recent_commits` count cap, applied when the request gives
/// NEITHER `limit` nor `since`, so the read always bounds rather than streaming
/// an unbounded merged log.
const DEFAULT_RECENT_COMMITS_LIMIT: usize = 100;

/// The author of a `recent_commits` row, name and email split so a consumer can
/// tell a human commit from an engine one (`au-engine <au-engine@arsumbris.ai>`).
#[derive(Debug, Serialize)]
struct AuthorDto {
    name: String,
    email: String,
}

/// One changed file in a `recent_commits` row, git's `--name-status -M` per file.
#[derive(Debug, Serialize)]
struct ChangedFileDto {
    path: String,
    /// `added` / `modified` / `deleted` / `renamed`. `renamed` is git's
    /// heuristic, not authoritative.
    status: &'static str,
    /// The prior path on a `renamed`, git's heuristic match; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
}

/// One commit in the `recent_commits` stream, keyed by its working tree.
#[derive(Debug, Serialize)]
struct RecentCommitDto {
    commit: String,
    /// The owning working-tree root, the lane key.
    tree: String,
    /// The au-repo member names living in that tree.
    members: Vec<String>,
    author: AuthorDto,
    /// Committer date, unix seconds.
    timestamp: i64,
    subject: String,
    changed_files: Vec<ChangedFileDto>,
    /// Every trailer line, uninterpreted. Empty, never absent.
    trailers: Vec<CommitTrailerDto>,
}

impl RecentCommitDto {
    fn from_row(row: crate::gitwriter::RecentCommitRow) -> Self {
        let r = row.record;
        Self {
            commit: r.commit,
            tree: row.tree,
            members: row.members,
            author: AuthorDto {
                name: r.author_name,
                email: r.author_email,
            },
            timestamp: r.timestamp,
            subject: r.subject,
            changed_files: r
                .changed_files
                .into_iter()
                .map(|f| ChangedFileDto {
                    path: f.path,
                    status: f.status,
                    from: f.from,
                })
                .collect(),
            trailers: r
                .trailers
                .into_iter()
                .map(|t| CommitTrailerDto {
                    key: t.key,
                    value: t.value,
                })
                .collect(),
        }
    }
}

/// The `recent_commits` read: resolve the workspace's working trees under the
/// state lock, then read and merge their logs off git.
///
/// Same lock/reactor boundary as `commit_meta`: the member -> tree mapping runs
/// under the state lock (in-memory), the per-tree `git log` runs on a blocking
/// task, off the reactor and off the lock. `members` filters by member name;
/// each maps to its covering working tree (a monorepo's members share one),
/// deduped. `limit` and `since` bound; with neither, a default cap applies.
async fn recent_commits(
    handle: &EngineHandle,
    members: Option<Vec<String>>,
    limit: Option<usize>,
    since: Option<String>,
) -> Response {
    let entry_root = handle.root().to_path_buf();
    let cache_root = handle.package_cache_root().map(Path::to_path_buf);
    // Under the lock: map members to their distinct working trees, the same
    // `git.root` per member `commit_meta` / `file_history` route by.
    let resolved = handle.read(move |v| {
        recent_commit_tree_pairs(v, &entry_root, cache_root.as_deref(), members.as_deref())
    });
    let (version, trees) = match resolved {
        Read::NotReady => return Response::not_ready(),
        Read::Ready { version, value } => (version, value),
    };
    // A time window is a bound on its own, so a default cap applies only when
    // neither `limit` nor `since` is given; the effective cap is both the
    // per-tree `-n` and the merge cut.
    let effective_limit = limit.or_else(|| since.is_none().then_some(DEFAULT_RECENT_COMMITS_LIMIT));
    let rows =
        tokio::task::spawn_blocking(move || recent_commits_git(trees, effective_limit, since))
            .await
            .expect("recent_commits task never panics");
    enveloped(
        "recent_commits",
        Read::Ready {
            version,
            value: rows,
        },
    )
}

/// The git half of `recent_commits`, off the reactor. Runs one `git log` per
/// tree, merges newest-first, cuts to the bound, and maps to the wire rows.
fn recent_commits_git(
    trees: Vec<(String, Vec<String>)>,
    limit: Option<usize>,
    since: Option<String>,
) -> Vec<RecentCommitDto> {
    let per_tree: Vec<(
        String,
        Vec<String>,
        Vec<crate::gitwriter::RecentCommitRecord>,
    )> = trees
        .into_iter()
        .map(|(tree, members)| {
            // A git failure for one tree degrades to an empty contribution, the
            // same as a non-git member, rather than failing the whole read.
            let records = crate::gitwriter::ShellGit
                .recent_commits(Path::new(&tree), limit, since.as_deref())
                .unwrap_or_default();
            (tree, members, records)
        })
        .collect();
    crate::gitwriter::merge_recent_commits(per_tree, limit)
        .into_iter()
        .map(RecentCommitDto::from_row)
        .collect()
}

/// The workspace's working trees (each `(tree root, member names)`), the
/// member -> tree map both the `recent_commits` read and its subscription
/// resolve under the read lock. `members` filters by member name; absent = all.
fn recent_commit_tree_pairs(
    v: &KnowledgeBase,
    entry_root: &Path,
    cache_root: Option<&Path>,
    members: Option<&[String]>,
) -> Vec<(String, Vec<String>)> {
    let view = wire::introspect_members(v, entry_root, cache_root);
    let pairs: Vec<(String, Option<String>)> = view
        .members
        .iter()
        .map(|m| (m.repo.clone(), m.git.root.clone()))
        .collect();
    crate::gitwriter::group_members_by_tree(&pairs, members)
}

/// Args for the `recent_commits` subscription, the same shape as the read.
#[derive(Debug, Deserialize)]
struct RecentCommitsSubArgs {
    #[serde(default)]
    members: Option<Vec<String>>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    since: Option<String>,
}

/// One tree's held state in a `recent_commits` subscription: its root, member
/// names, and its last-read commit records.
type TreeState = (
    String,
    Vec<String>,
    Vec<crate::gitwriter::RecentCommitRecord>,
);

/// Read each tree's log once, the subscription's seed and per-tree cache.
fn seed_trees(
    trees: Vec<(String, Vec<String>)>,
    cap: Option<usize>,
    since: Option<String>,
) -> Vec<TreeState> {
    trees
        .into_iter()
        .map(|(tree, members)| {
            let records = crate::gitwriter::ShellGit
                .recent_commits(Path::new(&tree), cap, since.as_deref())
                .unwrap_or_default();
            (tree, members, records)
        })
        .collect()
}

/// Re-read one tree's log after its reflog moved.
fn relog_tree(
    tree: String,
    cap: Option<usize>,
    since: Option<String>,
) -> Vec<crate::gitwriter::RecentCommitRecord> {
    crate::gitwriter::ShellGit
        .recent_commits(Path::new(&tree), cap, since.as_deref())
        .unwrap_or_default()
}

/// Every commit oid currently held across the per-tree caches, the set the
/// subscription treats as already accounted for.
///
/// The live diff runs against THIS union, never the bounded seed page: a commit
/// that fell below the seed's global cut is still here, so it is not
/// re-surfaced as newly-appeared on a multi-tree workspace. And because a commit
/// that ages out of every tree's `-n cap` window leaves the union (and can never
/// reappear in a future merge, which only draws from the caches), the set stays
/// bounded by the held records rather than growing per commit forever.
fn held_oids(per_tree: &[TreeState]) -> std::collections::HashSet<String> {
    per_tree
        .iter()
        .flat_map(|(_, _, recs)| recs.iter().map(|r| r.commit.clone()))
        .collect()
}

/// Merge the held per-tree states into the wire rows, newest-first, bounded.
fn merge_states_to_dtos(per_tree: &[TreeState], limit: Option<usize>) -> Vec<RecentCommitDto> {
    let cloned: Vec<_> = per_tree
        .iter()
        .map(|(t, m, r)| (t.clone(), m.clone(), r.clone()))
        .collect();
    crate::gitwriter::merge_recent_commits(cloned, limit)
        .into_iter()
        .map(RecentCommitDto::from_row)
        .collect()
}

/// Disarm a subscription's reflog watchers when its task ends, including on an
/// abort (Drop runs on the dropped future). Disarming a tree that never armed
/// (a non-git tree) is a no-op, so the guard is safe to arm before the watchers.
struct DisarmGuard {
    handle: EngineHandle,
    trees: Vec<PathBuf>,
}

impl Drop for DisarmGuard {
    fn drop(&mut self) {
        self.handle.disarm_git_watch(&self.trees);
    }
}

/// A `change_event` carrying the newly-appeared commits in its scope hint.
/// `at_version` is the held knowledge-base version at emit, a monotonic stamp,
/// NOT a git coherence cursor (the commit is off the version axis).
fn recent_commits_change_event_frame(
    subscription_id: u64,
    at_version: u64,
    commits: &[&RecentCommitDto],
) -> serde_json::Value {
    serde_json::json!({
        "type": "change_event",
        "schema_version": SCHEMA_VERSION,
        "subscription_id": subscription_id,
        "kind": "commits-appeared",
        "at_version": at_version,
        "scope_hint": { "scope": "recent_commits", "commits": commits },
    })
}

/// `recent_commits`: the seed page as the initial value, then a
/// `commits-appeared` change event carrying each newly-appeared commit. An APPEND-ONLY stream: the
/// server emits new commits, the client owns its bounded window. Rewrites are
/// out of scope (the append-only invariant), so no removals.
///
/// Liveness is the reflog watcher, NOT the version signal — a git-state
/// projection. The version signal is used ONCE, to wait for the member set to
/// become known; thereafter only reflog events wake the loop. The watchers are
/// armed before the seed (a commit in the arm-to-seed window is captured, and
/// idempotent by oid) and disarmed when the task ends.
async fn run_recent_commits(
    handle: EngineHandle,
    frame_tx: FrameTx,
    id: u64,
    args: RecentCommitsSubArgs,
) {
    let RecentCommitsSubArgs {
        members,
        limit,
        since,
    } = args;
    // Subscribe to reflog events BEFORE arming, so an event during arm/seed is
    // captured rather than lost.
    let mut git_rx = handle.subscribe_git_events();
    // Wait for readiness so the member -> tree map is known. This is the ONLY
    // use of the version signal here, an initial gate, never a re-log wake.
    let mut ver = handle.subscribe_version();
    while handle.version().is_none() {
        if ver.changed().await.is_err() {
            return;
        }
    }
    let entry_root = handle.root().to_path_buf();
    let cache_root = handle.package_cache_root().map(Path::to_path_buf);
    let members_r = members.clone();
    let trees = match handle.read(move |v| {
        recent_commit_tree_pairs(v, &entry_root, cache_root.as_deref(), members_r.as_deref())
    }) {
        Read::Ready { value, .. } => value,
        Read::NotReady => return, // raced back to Deriving; rare, give up.
    };
    let tree_roots: Vec<PathBuf> = trees.iter().map(|(t, _)| PathBuf::from(t)).collect();

    // Arm the watchers, then guard EXACTLY the subset actually armed, so the
    // teardown decrements only trees this subscription incremented — never a
    // skipped tree, which would decrement another subscription's count for it.
    // A task abort during the arm's `spawn_blocking` is a rare micro-leak (the
    // detached blocking arm finishes with no guard to disarm it); the common
    // path is correct, which the disarm-everything-requested alternative was not.
    let armed = {
        let h = handle.clone();
        let roots = tree_roots.clone();
        tokio::task::spawn_blocking(move || h.arm_git_watch(&roots))
            .await
            .unwrap_or_default()
    };
    let _guard = DisarmGuard {
        handle: handle.clone(),
        trees: armed,
    };

    let effective_limit = limit.or_else(|| since.is_none().then_some(DEFAULT_RECENT_COMMITS_LIMIT));

    // Seed: read every tree's log once, hold it, and emit the merged page.
    let mut per_tree = {
        let trees2 = trees.clone();
        let since2 = since.clone();
        tokio::task::spawn_blocking(move || seed_trees(trees2, effective_limit, since2))
            .await
            .expect("recent_commits seed task never panics")
    };
    let seed = merge_states_to_dtos(&per_tree, effective_limit);
    // The dedup set is EVERYTHING held across the trees, not just the bounded
    // seed page the client received: the live diff runs against the full
    // per-tree union, so a below-the-cut commit is already accounted for and is
    // never re-emitted as newly-appeared. See `held_oids`.
    let mut emitted = held_oids(&per_tree);
    let at = handle.version().unwrap_or(0);
    if !send_frame(
        &frame_tx,
        initial_value_frame(id, at, serde_json::to_value(&seed).expect("rows serialize")),
    )
    .await
    {
        return;
    }

    // Live loop: a reflog event re-logs the tree that moved (or all trees on a
    // lag), re-merges, and emits any commit not seen before. Additions only.
    loop {
        let moved = match git_rx.recv().await {
            Ok(tree) => Some(tree),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => None, // fall back to all trees.
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let to_relog: Vec<usize> = match &moved {
            Some(tree) => per_tree
                .iter()
                .position(|(t, _, _)| Path::new(t) == tree.as_path())
                .into_iter()
                .collect(),
            None => (0..per_tree.len()).collect(),
        };
        if to_relog.is_empty() {
            continue; // an event for a tree this subscription does not hold.
        }
        for i in to_relog {
            let tree = per_tree[i].0.clone();
            let since_i = since.clone();
            let records =
                tokio::task::spawn_blocking(move || relog_tree(tree, effective_limit, since_i))
                    .await
                    .expect("recent_commits relog task never panics");
            per_tree[i].2 = records;
        }
        // Merge unbounded for the ongoing stream (each tree is already capped),
        // so every newly-appeared commit is emitted, not just the newest page.
        let merged = merge_states_to_dtos(&per_tree, None);
        let new: Vec<&RecentCommitDto> = merged
            .iter()
            .filter(|d| !emitted.contains(&d.commit))
            .collect();
        // Rebase the dedup set onto everything now held. A commit below the
        // seed's global cut is already in `emitted` (so it is NOT re-surfaced as
        // newly-appeared, the multi-tree leak), and an oid that ages out of every
        // tree's window leaves the set (so it stays bounded).
        let held = held_oids(&per_tree);
        if new.is_empty() {
            emitted = held;
            continue;
        }
        let at = handle.version().unwrap_or(0);
        let frame = recent_commits_change_event_frame(id, at, &new);
        emitted = held;
        if !send_frame(&frame_tx, frame).await {
            return;
        }
    }
}

async fn preview_mutation(handle: &EngineHandle, args: PreviewMutationArgs) -> Response {
    let (path_arg, op, stamps) = args.into_parts();
    let root = handle.root().to_path_buf();
    // Resolve the path against the mounted members, with the mutation channel's
    // path rules AND the editability gate. A failure is a refusal, folded into the
    // reject channel, so preview reports the reject the real write would.
    let resolved = handle.read(move |v| resolve_editable_target(v, &root, &path_arg));
    let target = match resolved {
        Read::NotReady => return Response::not_ready(),
        Read::Ready {
            version,
            value: Err(reject),
        } => {
            return enveloped(
                "preview_mutation",
                Read::Ready {
                    version,
                    value: preview_reject_view(reject),
                },
            )
        }
        Read::Ready {
            value: Ok(target), ..
        } => target,
    };

    let input = crate::engine::PreviewInput { target, op, stamps };
    let h = handle.clone();
    let read = tokio::task::spawn_blocking(move || h.preview(input, preview_product_view))
        .await
        .expect("preview task never panics");
    enveloped("preview_mutation", read)
}

/// Assemble a preview's wire product from its outcome: the built product, or a
/// structural reject folded into the `reject` channel.
fn preview_product_view(outcome: crate::engine::PreviewOutcome) -> wire::PreviewProductView {
    match outcome {
        crate::engine::PreviewOutcome::Built {
            preview,
            held,
            target,
        } => wire::preview_product_view(preview, held, target),
        crate::engine::PreviewOutcome::Reject(reject) => preview_reject_view(reject),
    }
}

/// A `MutationReject` as a preview product's `reject` field.
fn preview_reject_view(reject: crate::mutate::MutationReject) -> wire::PreviewProductView {
    wire::PreviewProductView::Reject {
        reject: wire::PreviewRejectView {
            message: reject.message,
            detail: reject.detail,
        },
    }
}

fn mutation_response(
    handle: &EngineHandle,
    target: &Path,
    written_hash: Option<crate::ir::ContentHash>,
    commits: SagaCommits,
) -> serde_json::Value {
    mutation_response_extra(
        handle,
        target,
        written_hash,
        commits,
        serde_json::Map::new(),
    )
}

/// The success frame for a mutation: the read-response envelope, `version`
/// the post-mutation knowledge base version (the consumer's echo-suppression
/// correlation), the result carrying the file's identity, fresh diagnostics,
/// the saga's commits, and any primitive-specific `extra` fields.
///
/// Two commit fields, a clean anchor / provenance split ([[spec - git write
/// path - commit-per-mutation as a local saga over the workspace's materialized
/// repos]]):
/// - `commit`: HEAD of `target`'s own repo, the pin anchor for the file the
///   response is about. `null` only off-git. After a committing mutation it is
///   the new sha; after a no-op it is the unchanged HEAD. The read/mutate-
///   symmetric field, since the content read's `commit` is the same anchor (a
///   read never commits, so its `commit` is purely HEAD).
/// - `commits`: what THIS mutation committed, every repo keyed by name. `{}`
///   when nothing committed (off-git, or a no-op). Mutate-only provenance, a
///   read has none. `target`'s own repo is always among the committers when the
///   saga commits anything (the mutation's primary effect is on `target`), so a
///   non-empty `commits` means `commit` is the sha that just changed `target`.
///   Not first-of-N: `commit` follows `target`'s repo, never a different repo's
///   sha.
fn mutation_response_extra(
    handle: &EngineHandle,
    target: &Path,
    written_hash: Option<crate::ir::ContentHash>,
    commits: SagaCommits,
    extra: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    let scope = DiagnosticsScope {
        path: Some(target.to_path_buf()),
        ..Default::default()
    };
    let read = handle.read(|v| {
        let diagnostics: Vec<Diagnostic> = v
            .diagnostics()
            .filter(|d| scope.matches(v, d))
            .cloned()
            .collect();
        let held_hash = v.catalog.get(target).and_then(|e| e.hash);
        // The response file's own repo: its name keys the commits map, its root
        // is where a no-op falls back to reading live HEAD for the anchor.
        let own_repo = v
            .repos
            .repo_of(target)
            .map(|r| (r.name.clone(), r.root.clone()));
        (diagnostics, held_hash, own_repo)
    });
    let Read::Ready {
        version,
        value: (diagnostics, held_hash, own_repo),
    } = read
    else {
        return serde_json::to_value(Response::not_ready()).expect("response serializes");
    };
    // Does the held knowledge base reflect the write that produced this response? The
    // disk write is canonical; `rebuild()` is best-effort and can give up on
    // churn or a failing build, leaving the held catalog hash behind what was
    // written. `reflected: false` tells the consumer these diagnostics and
    // this hash predate its write, so it should await a later version rather
    // than trust them (or re-issue the mutation, which would conflict). With
    // no write to compare (an idempotent no-op), there is nothing to lag.
    let reflected = match written_hash {
        Some(written) => held_hash == Some(written),
        None => true,
    };
    // Project the saga's commits two ways. `commits` is the provenance: what
    // this mutation committed, per repo. `commit` is the anchor: HEAD of the
    // response file's own repo. When that repo committed, its new sha is already
    // in the map (no git call); on a no-op the map lacks it, so read live HEAD,
    // which is null only off-git.
    let commits_obj: serde_json::Map<String, serde_json::Value> = commits
        .iter()
        .map(|(repo, sha)| {
            (
                repo.as_str().to_string(),
                serde_json::Value::String(sha.0.clone()),
            )
        })
        .collect();
    let commit = match own_repo {
        Some((own_name, own_root)) => commits
            .iter()
            .find(|(repo, _)| *repo == own_name)
            .map(|(_, sha)| sha.0.clone())
            .or_else(|| ShellGit.head_commit(&own_root))
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    };
    let mut result = serde_json::Map::new();
    result.insert(
        "path".into(),
        serde_json::Value::String(target.display().to_string()),
    );
    result.insert(
        "hash".into(),
        serde_json::to_value(held_hash.map(crate::mutate::hash_hex)).expect("hash"),
    );
    result.insert(
        "diagnostics".into(),
        serde_json::to_value(diagnostics).expect("diagnostics serialize"),
    );
    result.insert("reflected".into(), serde_json::Value::Bool(reflected));
    result.insert("commit".into(), commit);
    result.insert("commits".into(), serde_json::Value::Object(commits_obj));
    result.extend(extra);
    serde_json::to_value(Response::ready(version, serde_json::Value::Object(result)))
        .expect("response serializes")
}

/// A rejected mutation: an `error` frame, nothing written. `detail` carries
/// machine-usable context (e.g. the current hash on a staleness conflict).
fn mutation_reject_frame(reject: crate::mutate::MutationReject) -> serde_json::Value {
    let mut frame = serde_json::json!({
        "type": "error",
        "schema_version": SCHEMA_VERSION,
        "error": reject.message,
    });
    if let Some(detail) = reject.detail {
        frame["detail"] = detail;
    }
    frame
}

/// Validate a mutation's caller attribution into commit trailers, or the
/// error frame to return. A reserved-key collision or a malformed trailer
/// rejects the whole mutation before any write, so nothing lands half-attributed.
fn resolve_attribution(
    input: &[TrailerInput],
) -> Result<Vec<crate::gitwriter::CommitTrailer>, serde_json::Value> {
    let pairs: Vec<(String, String)> = input
        .iter()
        .map(|t| (t.key.clone(), t.value.clone()))
        .collect();
    crate::gitwriter::validate_attribution(&pairs)
        .map_err(|msg| mutation_reject_frame(crate::mutate::MutationReject::new(msg)))
}

/// The ref's lifecycle as the `ready` read and channel both report it.
fn lifecycle_json(handle: &EngineHandle) -> serde_json::Value {
    let (engine_state, ref_state) = handle.lifecycle();
    serde_json::json!({
        "engine": format!("{engine_state:?}").to_lowercase(),
        "ref": format!("{ref_state:?}").to_lowercase(),
    })
}

/// Drive one subscription for the life of the connection: ack, then the initial
/// value when the channel has one, then a change event on every relevant
/// rebuild. Ends when a send fails (the connection went away) or the engine's
/// version signal closes.
async fn run_subscription(
    handle: EngineHandle,
    frame_tx: FrameTx,
    id: u64,
    request_id: Option<serde_json::Value>,
    sub: Subscription,
) {
    // Register the version receiver before the ack, so a rebuild landing
    // between the ack and the channel's first await cannot be missed: the
    // receiver already holds the pre-ack version as its seen mark.
    let rx = handle.subscribe_version();
    if !send_frame(&frame_tx, ack_frame(id, sub.channel(), &request_id)).await {
        return;
    }
    match sub {
        Subscription::Changes => run_changes(handle, frame_tx, id, rx).await,
        Subscription::Lifecycle => run_ready(handle, frame_tx, id, rx).await,
        Subscription::Types => run_types(handle, frame_tx, id, rx).await,
        Subscription::Files => run_files(handle, frame_tx, id, rx).await,
        Subscription::Diagnostics(args) => {
            let scope = args.resolve(handle.root());
            run_diagnostics(handle, frame_tx, id, scope, rx).await
        }
        Subscription::LinkGraph(args) => {
            let repo = args.repo;
            let scope = wire::TypeScope::new(resolved_scope(args.scope, &repo).own_only());
            run_link_graph(handle, frame_tx, id, rx, repo, scope).await
        }
        Subscription::TypeGraph(args) => {
            let repo = args.repo;
            let scope = wire::TypeScope::new(resolved_scope(args.scope, &repo).own_only());
            let classes = EdgeClasses::from_arg(args.edges);
            run_type_graph(handle, frame_tx, id, rx, repo, scope, classes).await
        }
        // Liveness is the reflog watcher, not the version signal, so the
        // version receiver `rx` is unused here; the task subscribes to git
        // events itself.
        Subscription::RecentCommits(args) => run_recent_commits(handle, frame_tx, id, args).await,
    }
}

/// `changes`: no initial value, a change event on every rebuild, carrying the
/// net file delta in its scope hint. The task holds the file→content-hash map
/// it last reported and diffs it on each wake, so coalesced rebuilds yield one
/// event covering everything since the subscriber's last delivery. A wake
/// whose net delta is empty (rebuilds that cancelled out) does not fire.
async fn run_changes(
    handle: EngineHandle,
    frame_tx: FrameTx,
    id: u64,
    mut rx: watch::Receiver<u64>,
) {
    // The baseline at subscribe time. Empty while Deriving, so a subscriber
    // arriving before the first build sees every file as added by it.
    let mut last = files_snapshot(&handle).map(|(_, s)| s).unwrap_or_default();
    loop {
        if rx.changed().await.is_err() {
            return;
        }
        let Some((version, cur)) = files_snapshot(&handle) else {
            continue;
        };
        let delta = files_delta(&last, &cur);
        if !delta.is_empty() {
            if !send_frame(
                &frame_tx,
                files_change_event_frame(id, "knowledge-base-changed", version, &delta),
            )
            .await
            {
                return;
            }
            last = cur;
        }
    }
}

/// The catalogued files and their content hashes, coherent at one version.
/// `None` while Deriving. The hash is `None` for files the build doesn't read
/// (assets), so their edits never appear as modifications — matching the
/// rebuild trigger, which fingerprints them by presence only.
fn files_snapshot(
    handle: &EngineHandle,
) -> Option<(u64, BTreeMap<PathBuf, Option<crate::ir::ContentHash>>)> {
    match handle.read(|v| v.catalog.iter().map(|(p, e)| (p.clone(), e.hash)).collect()) {
        Read::Ready { version, value } => Some((version, value)),
        Read::NotReady => None,
    }
}

/// The net file delta between two snapshots: paths that appeared, vanished,
/// or changed content. Lists are sorted (snapshot order).
struct FilesDelta {
    added: Vec<String>,
    removed: Vec<String>,
    modified: Vec<String>,
}

impl FilesDelta {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }
}

fn files_delta(
    last: &BTreeMap<PathBuf, Option<crate::ir::ContentHash>>,
    cur: &BTreeMap<PathBuf, Option<crate::ir::ContentHash>>,
) -> FilesDelta {
    let mut delta = FilesDelta {
        added: Vec::new(),
        removed: Vec::new(),
        modified: Vec::new(),
    };
    for (path, hash) in cur {
        match last.get(path) {
            None => delta.added.push(path.display().to_string()),
            Some(last_hash) if last_hash != hash => delta.modified.push(path.display().to_string()),
            _ => {}
        }
    }
    for path in last.keys() {
        if !cur.contains_key(path) {
            delta.removed.push(path.display().to_string());
        }
    }
    delta
}

/// A `change_event` frame whose scope hint carries a file delta.
fn files_change_event_frame(
    subscription_id: u64,
    kind: &str,
    at_version: u64,
    delta: &FilesDelta,
) -> serde_json::Value {
    serde_json::json!({
        "type": "change_event",
        "schema_version": SCHEMA_VERSION,
        "subscription_id": subscription_id,
        "kind": kind,
        "at_version": at_version,
        "scope_hint": {
            "scope": "files",
            "added": delta.added,
            "removed": delta.removed,
            "modified": delta.modified,
        },
    })
}

/// `ready`: the current lifecycle immediately, then a `ready-changed` event on
/// the Deriving-to-Ready transition.
async fn run_ready(handle: EngineHandle, frame_tx: FrameTx, id: u64, mut rx: watch::Receiver<u64>) {
    let at_version = handle.version().unwrap_or(0);
    let (_, ref_state) = handle.lifecycle();
    let mut was_ready = ref_state == RefState::Ready;
    if !send_frame(
        &frame_tx,
        initial_value_frame(
            id,
            at_version,
            serde_json::json!({ "lifecycle": lifecycle_json(&handle) }),
        ),
    )
    .await
    {
        return;
    }
    loop {
        if rx.changed().await.is_err() {
            return;
        }
        let (_, ref_state) = handle.lifecycle();
        let now_ready = ref_state == RefState::Ready;
        if now_ready && !was_ready {
            let version = handle.version().unwrap_or(*rx.borrow());
            if !send_frame(
                &frame_tx,
                change_event_frame(id, "lifecycle-changed", version),
            )
            .await
            {
                return;
            }
        }
        was_ready = now_ready;
    }
}

/// `files`: the catalogued paths as the initial value, then a
/// `files-changed` event when the path set differs across a rebuild,
/// the scope hint carrying the added and removed paths. The task holds the
/// path set it last reported, so coalesced rebuilds yield one net delta.
async fn run_files(handle: EngineHandle, frame_tx: FrameTx, id: u64, mut rx: watch::Receiver<u64>) {
    if wait_until_ready(&handle, &mut rx).await.is_none() {
        return;
    }
    let Some((version, mut last)) = files_snapshot(&handle) else {
        return;
    };
    let files: Vec<serde_json::Value> = last
        .keys()
        .map(|p| serde_json::json!({ "path": p.display().to_string() }))
        .collect();
    if !send_frame(
        &frame_tx,
        initial_value_frame(id, version, serde_json::Value::Array(files)),
    )
    .await
    {
        return;
    }
    loop {
        if rx.changed().await.is_err() {
            return;
        }
        let Some((version, cur)) = files_snapshot(&handle) else {
            continue;
        };
        let added: Vec<String> = cur
            .keys()
            .filter(|p| !last.contains_key(*p))
            .map(|p| p.display().to_string())
            .collect();
        let removed: Vec<String> = last
            .keys()
            .filter(|p| !cur.contains_key(*p))
            .map(|p| p.display().to_string())
            .collect();
        if !(added.is_empty() && removed.is_empty()) {
            let frame = serde_json::json!({
                "type": "change_event",
                "schema_version": SCHEMA_VERSION,
                "subscription_id": id,
                "kind": "files-changed",
                "at_version": version,
                "scope_hint": { "scope": "files", "added": added, "removed": removed },
            });
            if !send_frame(&frame_tx, frame).await {
                return;
            }
            last = cur;
        }
    }
}

/// `types`: the workspace-wide type-def introspection as the initial value, the
/// same owner-deduped owner-annotated set the `types` read serves, then a
/// `types-changed` event when any type-def's wire introspection differs
/// across a rebuild, the scope hint naming the added, removed, and changed
/// types. Diffing the projection is more precise than the old type-def-file
/// content token: a file edit that leaves every introspection unchanged does
/// not fire.
async fn run_types(handle: EngineHandle, frame_tx: FrameTx, id: u64, mut rx: watch::Receiver<u64>) {
    if wait_until_ready(&handle, &mut rx).await.is_none() {
        return;
    }
    let Some((version, initial, mut last)) = types_snapshot(&handle) else {
        return;
    };
    if !send_frame(&frame_tx, initial_value_frame(id, version, initial)).await {
        return;
    }
    loop {
        if rx.changed().await.is_err() {
            return;
        }
        let Some((version, _, cur)) = types_snapshot(&handle) else {
            continue;
        };
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut changed = Vec::new();
        // A scope-hint entry is the type's identity handle, `{ name, repo, hash }`,
        // not a bare name, so a consumer knows WHICH same-named type changed.
        let entry = |key: &(String, String), intro: &serde_json::Value| {
            serde_json::json!({
                "name": key.0,
                "repo": key.1,
                "hash": intro.get("hash").cloned().unwrap_or(serde_json::Value::Null),
            })
        };
        for (key, intro) in &cur {
            match last.get(key) {
                None => added.push(entry(key, intro)),
                Some(last_intro) if last_intro != intro => changed.push(entry(key, intro)),
                _ => {}
            }
        }
        for (key, intro) in &last {
            if !cur.contains_key(key) {
                removed.push(entry(key, intro));
            }
        }
        if !(added.is_empty() && removed.is_empty() && changed.is_empty()) {
            let frame = serde_json::json!({
                "type": "change_event",
                "schema_version": SCHEMA_VERSION,
                "subscription_id": id,
                "kind": "types-changed",
                "at_version": version,
                "scope_hint": {
                    "scope": "types",
                    "added": added,
                    "removed": removed,
                    "changed": changed,
                },
            });
            if !send_frame(&frame_tx, frame).await {
                return;
            }
            last = cur;
        }
    }
}

/// The type graph coherent at one version: the `types` read's array shape for
/// the wire, plus the same introspections keyed by type name for the change
/// diff. `None` while Deriving.
fn types_snapshot(
    handle: &EngineHandle,
) -> Option<(
    u64,
    serde_json::Value,
    BTreeMap<(String, String), serde_json::Value>,
)> {
    match handle.read(|v| wire::introspect_workspace_types(v, wire::TypeScope::all())) {
        Read::Ready { version, value } => {
            // Key by IDENTITY, `(name, owner repo)`, not bare name. Two mounted
            // repos can each own a same-named type, so a bare-name key collides
            // and a change to the shadowed one is silently dropped.
            let by_id = value
                .iter()
                .map(|t| {
                    (
                        (t.def.name.clone(), t.repo.clone()),
                        serde_json::to_value(t).expect("type serializes"),
                    )
                })
                .collect();
            let array = serde_json::to_value(&value).expect("types serialize");
            Some((version, array, by_id))
        }
        Read::NotReady => None,
    }
}

/// Park on the version receiver until the ref is Ready, returning the version
/// then. `None` if the engine's version channel closes first.
async fn wait_until_ready(handle: &EngineHandle, rx: &mut watch::Receiver<u64>) -> Option<u64> {
    loop {
        if let Some(version) = handle.version() {
            return Some(version);
        }
        if rx.changed().await.is_err() {
            return None;
        }
    }
}

/// `diagnostics`: the in-scope diagnostics as the initial value, then a
/// `diagnostics-changed` event whenever the in-scope set differs across a
/// rebuild. The coarse delta tokens don't carry per-file detail under the
/// full-rebuild engine, so the task holds the last delivered set and diffs
/// it on each version advance.
async fn run_diagnostics(
    handle: EngineHandle,
    frame_tx: FrameTx,
    id: u64,
    scope: DiagnosticsScope,
    mut rx: watch::Receiver<u64>,
) {
    if wait_until_ready(&handle, &mut rx).await.is_none() {
        return;
    }
    let Some((version, flat, mut last_by_file)) = diagnostics_snapshot(&handle, &scope) else {
        return;
    };
    if !send_frame(&frame_tx, initial_value_frame(id, version, flat)).await {
        return;
    }
    loop {
        if rx.changed().await.is_err() {
            return;
        }
        let Some((version, _, cur_by_file)) = diagnostics_snapshot(&handle, &scope) else {
            continue;
        };
        let changed = changed_files(&last_by_file, &cur_by_file);
        if !changed.is_empty() {
            if !send_frame(
                &frame_tx,
                diagnostics_change_event_frame(id, version, &changed),
            )
            .await
            {
                return;
            }
            last_by_file = cur_by_file;
        }
    }
}

/// A coherent read of the diagnostics in the subscription's scope.
///
/// Returns the version, the flat array in the `diagnostics` read's shape and
/// knowledge base order for the wire, plus the same diagnostics grouped by file for
/// the change diff. An empty scope is whole-knowledge-base. `None` while Deriving.
fn diagnostics_snapshot(
    handle: &EngineHandle,
    scope: &DiagnosticsScope,
) -> Option<(u64, serde_json::Value, BTreeMap<String, serde_json::Value>)> {
    match handle.read(|v| {
        let filtered: Vec<&Diagnostic> = scoped_diagnostics(v, scope).collect();
        let flat = serde_json::to_value(&filtered).expect("diagnostics serialize");
        let mut by_file: BTreeMap<String, Vec<&Diagnostic>> = BTreeMap::new();
        for d in &filtered {
            by_file
                .entry(d.span.file.display().to_string())
                .or_default()
                .push(d);
        }
        let by_file = by_file
            .into_iter()
            .map(|(file, ds)| {
                (
                    file,
                    serde_json::to_value(ds).expect("diagnostics serialize"),
                )
            })
            .collect();
        (flat, by_file)
    }) {
        Read::Ready {
            version,
            value: (flat, by_file),
        } => Some((version, flat, by_file)),
        Read::NotReady => None,
    }
}

/// The files whose diagnostic set differs between two snapshots: changed,
/// added, or cleared. Sorted, deduplicated.
fn changed_files(
    last: &BTreeMap<String, serde_json::Value>,
    cur: &BTreeMap<String, serde_json::Value>,
) -> Vec<String> {
    let mut changed = Vec::new();
    for (file, diags) in cur {
        if last.get(file) != Some(diags) {
            changed.push(file.clone());
        }
    }
    for file in last.keys() {
        if !cur.contains_key(file) {
            changed.push(file.clone());
        }
    }
    changed.sort();
    changed.dedup();
    changed
}

/// A `diagnostics-changed` event. The scope hint is precise here, unlike the
/// coarse knowledge base hint other channels carry: the engine diffs diagnostics by
/// file, so it names exactly the files whose diagnostic set changed.
fn diagnostics_change_event_frame(
    subscription_id: u64,
    at_version: u64,
    files: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "type": "change_event",
        "schema_version": SCHEMA_VERSION,
        "subscription_id": subscription_id,
        "kind": "diagnostics-changed",
        "at_version": at_version,
        "scope_hint": { "scope": "files", "files": files },
    })
}

/// `link_graph`: the full node+edge payload as the initial value, then a node/
/// edge DELTA per rebuild whose net change is non-empty. The task holds the
/// node and edge sets it last reported and diffs them on each wake, so a rebuild
/// that leaves the graph unchanged in scope does not fire. The same
/// hold-last-snapshot-and-diff shape `changes` / `files` / `diagnostics` use.
async fn run_link_graph(
    handle: EngineHandle,
    frame_tx: FrameTx,
    id: u64,
    mut rx: watch::Receiver<u64>,
    repo: Option<String>,
    scope: wire::TypeScope,
) {
    if wait_until_ready(&handle, &mut rx).await.is_none() {
        return;
    }
    let Some((version, mut last)) = link_graph_snapshot(&handle, repo.as_deref(), scope) else {
        return;
    };
    if !send_frame(
        &frame_tx,
        initial_value_frame(
            id,
            version,
            serde_json::to_value(&last).expect("link_graph serializes"),
        ),
    )
    .await
    {
        return;
    }
    loop {
        if rx.changed().await.is_err() {
            return;
        }
        let Some((version, cur)) = link_graph_snapshot(&handle, repo.as_deref(), scope) else {
            continue;
        };
        let delta = link_graph_delta(&last, &cur);
        if !delta.is_empty() {
            if !send_frame(
                &frame_tx,
                link_graph_change_event_frame(id, version, &delta),
            )
            .await
            {
                return;
            }
            last = cur;
        }
    }
}

/// A coherent read of the whole-graph payload in the subscription's scope.
/// `None` while Deriving.
fn link_graph_snapshot(
    handle: &EngineHandle,
    repo: Option<&str>,
    scope: wire::TypeScope,
) -> Option<(u64, LinkGraphView)> {
    match handle.read(|v| link_graph_view(v, repo, scope)) {
        Read::Ready { version, value } => Some((version, value)),
        Read::NotReady => None,
    }
}

/// The net change between two whole-graph payloads. Nodes diff by path: a node
/// is `added` when its path is new OR its record changed (a re-referenced file
/// re-emits with new ref counts), `removed` when its path is gone. Edges diff as
/// a MULTISET over the sorted edge lists, so preserved multiplicity in the read
/// carries through to the delta.
#[derive(Debug, Serialize)]
struct LinkGraphDelta {
    nodes_added: Vec<GraphNodeView>,
    nodes_removed: Vec<String>,
    edges_added: Vec<GraphEdgeView>,
    edges_removed: Vec<GraphEdgeView>,
}

impl LinkGraphDelta {
    fn is_empty(&self) -> bool {
        self.nodes_added.is_empty()
            && self.nodes_removed.is_empty()
            && self.edges_added.is_empty()
            && self.edges_removed.is_empty()
    }
}

fn link_graph_delta(last: &LinkGraphView, cur: &LinkGraphView) -> LinkGraphDelta {
    let last_nodes: std::collections::HashMap<&str, &GraphNodeView> =
        last.nodes.iter().map(|n| (n.path.as_str(), n)).collect();
    let cur_nodes: std::collections::HashMap<&str, &GraphNodeView> =
        cur.nodes.iter().map(|n| (n.path.as_str(), n)).collect();
    let nodes_added: Vec<GraphNodeView> = cur
        .nodes
        .iter()
        .filter(|n| last_nodes.get(n.path.as_str()).is_none_or(|old| *old != *n))
        .cloned()
        .collect();
    let nodes_removed: Vec<String> = last
        .nodes
        .iter()
        .filter(|n| !cur_nodes.contains_key(n.path.as_str()))
        .map(|n| n.path.clone())
        .collect();

    let (edges_added, edges_removed) = edge_multiset_diff(&last.edges, &cur.edges);

    LinkGraphDelta {
        nodes_added,
        nodes_removed,
        edges_added,
        edges_removed,
    }
}

/// The multiset difference of two edge lists, each already sorted by
/// `(from, to, kind, surface)`. A merge walk pairs equal edges, so surplus in
/// `cur` is `added` and surplus in `last` is `removed`, multiplicity-correct
/// (two identical edges dropped to one yields one `removed`).
fn edge_multiset_diff(
    last: &[GraphEdgeView],
    cur: &[GraphEdgeView],
) -> (Vec<GraphEdgeView>, Vec<GraphEdgeView>) {
    let key = |e: &GraphEdgeView| {
        (
            e.from.clone(),
            e.to.clone(),
            e.kind.to_string(),
            e.surface.to_string(),
        )
    };
    let (mut i, mut j) = (0usize, 0usize);
    let (mut added, mut removed) = (Vec::new(), Vec::new());
    while i < last.len() && j < cur.len() {
        match key(&last[i]).cmp(&key(&cur[j])) {
            std::cmp::Ordering::Less => {
                removed.push(last[i].clone());
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                added.push(cur[j].clone());
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    removed.extend(last[i..].iter().cloned());
    added.extend(cur[j..].iter().cloned());
    (added, removed)
}

/// A `change_event` frame carrying the whole-graph node/edge delta in its scope
/// hint, `link_graph` scope.
fn link_graph_change_event_frame(
    subscription_id: u64,
    at_version: u64,
    delta: &LinkGraphDelta,
) -> serde_json::Value {
    serde_json::json!({
        "type": "change_event",
        "schema_version": SCHEMA_VERSION,
        "subscription_id": subscription_id,
        "kind": "link-graph-changed",
        "at_version": at_version,
        "scope_hint": {
            "scope": "link_graph",
            "nodes_added": delta.nodes_added,
            "nodes_removed": delta.nodes_removed,
            "edges_added": delta.edges_added,
            "edges_removed": delta.edges_removed,
        },
    })
}

/// `type_graph`: the schema-graph payload as the initial value, then a node/edge
/// DELTA per rebuild whose net change is non-empty. The type-side sibling of
/// [`run_link_graph`], the same hold-last-snapshot-and-diff shape.
async fn run_type_graph(
    handle: EngineHandle,
    frame_tx: FrameTx,
    id: u64,
    mut rx: watch::Receiver<u64>,
    repo: Option<String>,
    scope: wire::TypeScope,
    classes: EdgeClasses,
) {
    if wait_until_ready(&handle, &mut rx).await.is_none() {
        return;
    }
    let Some((version, mut last)) = type_graph_snapshot(&handle, repo.as_deref(), scope, classes)
    else {
        return;
    };
    if !send_frame(
        &frame_tx,
        initial_value_frame(
            id,
            version,
            serde_json::to_value(&last).expect("type_graph serializes"),
        ),
    )
    .await
    {
        return;
    }
    loop {
        if rx.changed().await.is_err() {
            return;
        }
        let Some((version, cur)) = type_graph_snapshot(&handle, repo.as_deref(), scope, classes)
        else {
            continue;
        };
        let delta = type_graph_delta(&last, &cur);
        if !delta.is_empty() {
            if !send_frame(
                &frame_tx,
                type_graph_change_event_frame(id, version, &delta),
            )
            .await
            {
                return;
            }
            last = cur;
        }
    }
}

/// A coherent read of the type-graph payload in the subscription's scope.
/// `None` while Deriving.
fn type_graph_snapshot(
    handle: &EngineHandle,
    repo: Option<&str>,
    scope: wire::TypeScope,
    classes: EdgeClasses,
) -> Option<(u64, TypeGraphView)> {
    match handle.read(|v| type_graph_view(v, repo, scope, classes)) {
        Read::Ready { version, value } => Some((version, value)),
        Read::NotReady => None,
    }
}

/// The net change between two type-graph payloads. Nodes diff by path (a node
/// re-emits when its record changes); edges are unique per
/// `(from, to, relation)`, so an edge re-emits when its `count` changes.
#[derive(Debug, Serialize)]
struct TypeGraphDelta {
    nodes_added: Vec<TypeGraphNodeView>,
    nodes_removed: Vec<String>,
    edges_added: Vec<TypeGraphEdgeView>,
    edges_removed: Vec<TypeGraphEdgeView>,
}

impl TypeGraphDelta {
    fn is_empty(&self) -> bool {
        self.nodes_added.is_empty()
            && self.nodes_removed.is_empty()
            && self.edges_added.is_empty()
            && self.edges_removed.is_empty()
    }
}

fn type_graph_delta(last: &TypeGraphView, cur: &TypeGraphView) -> TypeGraphDelta {
    use std::collections::HashMap;
    let last_nodes: HashMap<&str, &TypeGraphNodeView> =
        last.nodes.iter().map(|n| (n.path.as_str(), n)).collect();
    let cur_nodes: HashMap<&str, &TypeGraphNodeView> =
        cur.nodes.iter().map(|n| (n.path.as_str(), n)).collect();
    let nodes_added: Vec<TypeGraphNodeView> = cur
        .nodes
        .iter()
        .filter(|n| last_nodes.get(n.path.as_str()).is_none_or(|old| *old != *n))
        .cloned()
        .collect();
    let nodes_removed: Vec<String> = last
        .nodes
        .iter()
        .filter(|n| !cur_nodes.contains_key(n.path.as_str()))
        .map(|n| n.path.clone())
        .collect();

    let key = |e: &TypeGraphEdgeView| (e.from.clone(), e.to.clone(), e.relation);
    let last_edges: HashMap<(String, String, &'static str), &TypeGraphEdgeView> =
        last.edges.iter().map(|e| (key(e), e)).collect();
    let cur_edges: HashMap<(String, String, &'static str), &TypeGraphEdgeView> =
        cur.edges.iter().map(|e| (key(e), e)).collect();
    let edges_added: Vec<TypeGraphEdgeView> = cur
        .edges
        .iter()
        .filter(|e| last_edges.get(&key(e)).is_none_or(|old| *old != *e))
        .cloned()
        .collect();
    let edges_removed: Vec<TypeGraphEdgeView> = last
        .edges
        .iter()
        .filter(|e| !cur_edges.contains_key(&key(e)))
        .cloned()
        .collect();

    TypeGraphDelta {
        nodes_added,
        nodes_removed,
        edges_added,
        edges_removed,
    }
}

/// A `change_event` frame carrying the type-graph node/edge delta, `type_graph`
/// scope. The event kind stays kebab (`type-graph-changed`), reassigned from the
/// old introspection stream (now `types-changed`) to this drawable stream.
fn type_graph_change_event_frame(
    subscription_id: u64,
    at_version: u64,
    delta: &TypeGraphDelta,
) -> serde_json::Value {
    serde_json::json!({
        "type": "change_event",
        "schema_version": SCHEMA_VERSION,
        "subscription_id": subscription_id,
        "kind": "type-graph-changed",
        "at_version": at_version,
        "scope_hint": {
            "scope": "type_graph",
            "nodes_added": delta.nodes_added,
            "nodes_removed": delta.nodes_removed,
            "edges_added": delta.edges_added,
            "edges_removed": delta.edges_removed,
        },
    })
}

async fn handle_request(handle: &EngineHandle, req: Request, shutdown_tx: &ShutdownTx) -> Response {
    match req {
        Request::PreviewMutation(args) => preview_mutation(handle, args).await,
        Request::Shutdown => {
            let _ = shutdown_tx.send(true);
            // `ready` tracks the ref's lifecycle so it stays coupled to
            // `version` (present exactly when Ready), like the `ready` probe.
            // The ack rides `result`, present regardless of readiness.
            let (_, ref_state) = handle.lifecycle();
            Response {
                kind: "response",
                schema_version: SCHEMA_VERSION,
                id: None,
                ready: ref_state == RefState::Ready,
                version: handle.version(),
                result: Some(serde_json::json!({ "shutting_down": true })),
                error: None,
            }
        }
        Request::Lifecycle => {
            let (_, ref_state) = handle.lifecycle();
            let ready = ref_state == RefState::Ready;
            Response {
                kind: "response",
                schema_version: SCHEMA_VERSION,
                id: None,
                ready,
                version: handle.version(),
                // The lifecycle probe answers even while not ready, so it wraps
                // its payload here rather than through `enveloped`.
                result: Some(serde_json::json!({ "lifecycle": lifecycle_json(handle) })),
                error: None,
            }
        }
        Request::Diagnostics(args) => {
            // Page over the served stream's stable order (by source, then
            // position). No total rides the array; a consumer pages until a
            // short page, or asks `diagnostic_counts` for the total.
            let offset = args.offset.unwrap_or(0);
            let limit = args.limit;
            let scope = args.resolve(handle.root());
            enveloped(
                "diagnostics",
                handle.read(move |v| {
                    scoped_diagnostics(v, &scope)
                        .skip(offset)
                        .take(limit.unwrap_or(usize::MAX))
                        .cloned()
                        .collect::<Vec<_>>()
                }),
            )
        }
        Request::DiagnosticCounts(args) => {
            // Counts over the FULL filtered set, paging ignored. The shape of
            // the problem without materializing every entry.
            let scope = args.resolve(handle.root());
            enveloped(
                "diagnostic_counts",
                handle.read(move |v| diagnostic_counts_view(v, &scope)),
            )
        }
        Request::Types(args) => {
            let offset = args.offset.unwrap_or(0);
            let limit = args.limit;
            let summary = args.summary;
            let repo = args.repo;
            let own = args.scope.own_only();
            enveloped(
                "types",
                handle.read(move |v| {
                    let scope = wire::TypeScope::new(own);
                    match repo.as_deref() {
                        None => Some(wire::introspect_workspace_types_paged(
                            v, offset, limit, summary, scope,
                        )),
                        Some(name) => wire::introspect_repo_types_paged(
                            v, name, offset, limit, summary, scope,
                        ),
                    }
                }),
            )
        }
        Request::TypeCounts(args) => {
            let repo = args.repo;
            let own = args.scope.own_only();
            enveloped(
                "type_counts",
                handle.read(move |v| {
                    let scope = wire::TypeScope::new(own);
                    wire::introspect_type_counts(v, repo.as_deref(), scope)
                }),
            )
        }
        Request::TypeTree { scope } => {
            let own = scope.own_only();
            enveloped(
                "type_tree",
                handle.read(move |v| wire::introspect_type_tree(v, wire::TypeScope::new(own))),
            )
        }
        Request::Type(TypeArgs { name }) => enveloped(
            "type",
            handle.read(move |v| wire::introspect_type_by_name(v, &name)),
        ),
        Request::TypeBatch(TypeBatchArgs { names }) => enveloped(
            "type_batch",
            handle.read(move |v| wire::introspect_types_named(v, &names)),
        ),
        Request::InstancesOf(InstancesOfArgs {
            type_name,
            origins,
            instance,
            body,
        }) => instances_of_response(handle, type_name, origins, instance, body).await,
        Request::Imports { scope } => {
            let own = scope.own_only();
            enveloped(
                "imports",
                handle.read(move |v| wire::introspect_list_imports(v, wire::TypeScope::new(own))),
            )
        }
        Request::Subtypes { base, scope } => {
            let own = scope.own_only();
            to_response(
                handle
                    .read(move |v| wire::introspect_subtypes(v, &base, wire::TypeScope::new(own))),
            )
        }
        Request::Instances => to_response(handle.read(|v| InstancesView {
            count: v.instances.size(),
            aborted_at_load: v.any_aborted(),
            instances: wire::introspect_kb_instances(v),
        })),
        Request::Candidates(args) => {
            let offset = args.offset.unwrap_or(0);
            let limit = args.limit;
            let summary = args.summary;
            to_response(handle.read(move |v| CandidatesView {
                aborted_at_load: v.any_aborted(),
                candidates: wire::introspect_kb_candidates_paged(v, offset, limit, summary),
            }))
        }
        Request::CandidateCounts(_) => enveloped(
            "candidate_counts",
            handle.read(|v| wire::introspect_candidate_counts(v)),
        ),
        Request::InstanceCounts(args) => {
            let repo = args.repo;
            let own = args.scope.own_only();
            enveloped(
                "instance_counts",
                handle.read(move |v| {
                    let scope = wire::TypeScope::new(own);
                    wire::introspect_instance_counts(v, repo.as_deref(), scope)
                }),
            )
        }
        Request::Instance { path } => {
            let target = handle.root().join(&path);
            enveloped("instance", handle.read(|v| resolved_view(v, &target)))
        }
        Request::ReferencesIn { path } => {
            let target = handle.root().join(&path);
            enveloped("references_in", handle.read(|v| backlinks_view(v, &target)))
        }
        Request::ReferencesOut { path } => {
            let target = abs_arg(handle.root(), &path);
            enveloped("references_out", handle.read(|v| outgoing_view(v, &target)))
        }
        Request::Pins {
            target,
            source_type,
        } => enveloped(
            "pins",
            handle.read(move |v| pins_view(v, &target, &source_type)),
        ),
        Request::CommitMeta { commits } => commit_meta(handle, commits).await,
        Request::FileHistory { path, repo } => file_history(handle, path, repo).await,
        Request::RecentCommits {
            members,
            limit,
            since,
        } => recent_commits(handle, members, limit, since).await,
        Request::Neighborhood(args) => {
            let seed = abs_arg(handle.root(), &args.path);
            // Cross-field validations serde cannot express, each an error frame.
            // The allowed kinds are the walk's own vocabulary, not a copy.
            use crate::backlinks::WALK_KINDS;
            if let Some(ks) = &args.kinds {
                if let Some(bad) = ks.iter().find(|k| !WALK_KINDS.contains(&k.as_str())) {
                    return Response::invalid(format!(
                        "unknown edge kind '{bad}'; expected one of {}",
                        WALK_KINDS.join(" / ")
                    ));
                }
            }
            // Past depth 1 an unfiltered walk explodes through navigational
            // fan-out, so a `kinds` set is required.
            if args.depth > 1 && args.kinds.is_none() {
                return Response::invalid(format!(
                    "`kinds` is required past depth 1 (an unfiltered deep walk \
                     explodes through navigational fan-out); name one or more of {}",
                    WALK_KINDS.join(" / ")
                ));
            }
            // The node cap floor is 1 (the seed always counts); 0 would return
            // the seed and set `truncated`, a contradiction. Reject it.
            if args.max_nodes == Some(0) {
                return Response::invalid("`max_nodes` must be at least 1; the seed always counts");
            }
            neighborhood_response(handle, seed, args).await
        }
        Request::ResolveTarget { target, origin } => {
            let origin = origin.map(|o| abs_arg(handle.root(), &o));
            enveloped(
                "resolve_target",
                handle.read(|v| resolve_target_view(v, &target, origin.as_deref())),
            )
        }
        Request::ResolveBlockId {
            target,
            block_id,
            origin,
        } => {
            let origin = origin.map(|o| abs_arg(handle.root(), &o));
            enveloped(
                "resolve_block_id",
                handle.read(|v| resolve_block_id_view(v, &target, &block_id, origin.as_deref())),
            )
        }
        Request::ResolveAnchor {
            target,
            anchor,
            origin,
        } => {
            let origin = origin.map(|o| abs_arg(handle.root(), &o));
            enveloped(
                "resolve_anchor",
                handle.read(|v| resolve_anchor_view(v, &target, &anchor, origin.as_deref())),
            )
        }
        Request::Anchors { target, origin } => {
            let origin = origin.map(|o| abs_arg(handle.root(), &o));
            enveloped(
                "anchors",
                handle.read(|v| anchors_view(v, &target, origin.as_deref())),
            )
        }
        Request::BlockIds { target, origin } => {
            let origin = origin.map(|o| abs_arg(handle.root(), &o));
            enveloped(
                "block_ids",
                handle.read(|v| block_ids_view(v, &target, origin.as_deref())),
            )
        }
        Request::Files(args) => {
            let repo = args.repo.clone();
            let scope = wire::TypeScope::new(args.scope.own_only());
            let limit = args.limit.unwrap_or(usize::MAX);
            let offset = args.offset.unwrap_or(0);
            enveloped(
                "files",
                handle.read(move |v| files_view(v, repo.as_deref(), scope, limit, offset)),
            )
        }
        Request::DirEntries { dir } => {
            let root = handle.root().to_path_buf();
            let abs = abs_arg(&root, &dir);
            enveloped("dir_entries", handle.read(move |v| children_view(v, &abs)))
        }
        Request::Frontmatter { path } => {
            let target = abs_arg(handle.root(), &path);
            enveloped(
                "frontmatter",
                handle.read(move |v| wire::file_frontmatter(v, &target)),
            )
        }
        Request::Content { path } => {
            // Content reads source text from disk — the sanctioned exception to
            // reads-over-held-state, the working tree is the materialised ref.
            // The hash rides with the content from ONE read of the bytes, so a
            // consumer that read a file always holds a guard-usable
            // `expected_hash`: the same FNV-1a `write_file`/`delete_file`
            // re-hash on disk, present whenever the file is readable, with no
            // dependence on catalog warmth. Deriving both from one buffer keeps
            // them coherent; a second read would open a TOCTOU window.
            // Capture readiness and version under the held lock, drop it, then
            // read off the lock and off the executor, so disk IO never blocks
            // either the held state or a runtime worker.
            let target = abs_arg(handle.root(), &path);
            match handle.read(|_| ()) {
                Read::NotReady => Response::not_ready(),
                Read::Ready { version, .. } => {
                    let result = tokio::task::spawn_blocking(move || read_content_at(&target))
                        .await
                        .unwrap_or(None);
                    Response::ready(
                        version,
                        serde_json::json!({
                            "content": serde_json::to_value(result).expect("result serializes")
                        }),
                    )
                }
            }
        }
        Request::SemanticTokens { path } => {
            let target = abs_arg(handle.root(), &path);
            enveloped(
                "semantic_tokens",
                handle.read(|v| semantic_tokens_view(v, &target)),
            )
        }
        Request::ValidateValue {
            type_name,
            value,
            repo,
        } => enveloped(
            "validate_value",
            handle.read(move |v| {
                crate::value_validate::validate_value_verdicts(
                    v,
                    &type_name,
                    &value,
                    repo.as_deref(),
                )
            }),
        ),
        Request::TopLevelDirs(TopLevelDirsArgs { repo, scope }) => {
            let own = resolved_scope(scope, &repo).own_only();
            enveloped(
                "top_level_dirs",
                handle.read(move |v| {
                    top_level_dirs_view(v, repo.as_deref(), wire::TypeScope::new(own))
                }),
            )
        }
        Request::Members(MembersArgs { repo }) => {
            let root = handle.root().to_path_buf();
            let cache_root = handle.package_cache_root().map(Path::to_path_buf);
            enveloped(
                "members",
                handle.read(move |v| {
                    // Same `repo` filter `overview.members` applies, so the two
                    // agree by construction and the mirror holds for this field.
                    wire::introspect_members(v, &root, cache_root.as_deref())
                        .members
                        .into_iter()
                        .filter(|m| repo.as_deref().is_none_or(|want| m.repo == want))
                        .collect::<Vec<_>>()
                }),
            )
        }
        Request::Overview(args) => {
            // The up-front orientation map: each sub-part is one call to its
            // own read with these same args, aggregated in one read closure so
            // the whole map is coherent at one version. That mirror property is
            // load-bearing, not bookkeeping — taking it seriously is what
            // caught that `diagnostic_counts` could not express repo scoping at
            // all and that `hubs` had no read whatsoever.
            let root = handle.root().to_path_buf();
            let cache_root = handle.package_cache_root().map(Path::to_path_buf);
            let repo = args.repo;
            let resolved = resolved_scope(args.scope, &repo);
            let scope = wire::TypeScope::new(resolved.own_only());
            let scope_str = if resolved.own_only() { "own" } else { "all" };
            let echo_repo = repo.clone();
            let diag_scope = DiagnosticsScope {
                repo: repo.clone(),
                scope,
                ..Default::default()
            };
            enveloped(
                "overview",
                handle.read(move |v| OverviewView {
                    repo: echo_repo.clone(),
                    scope: scope_str,
                    // `members` honors `repo` but NOT `scope`: it is the
                    // TOPOLOGY field, and `scope` filters CONTENT. Filtering it
                    // would hide the dependency set from the one field whose
                    // job is to report it, and `members.editable` is exactly
                    // how a consumer sees which repos `own` selected.
                    members: wire::introspect_members(v, &root, cache_root.as_deref())
                        .members
                        .into_iter()
                        .filter(|m| repo.as_deref().is_none_or(|want| m.repo == want))
                        .collect(),
                    top_level_dirs: top_level_dirs_view(v, repo.as_deref(), scope),
                    type_counts: wire::introspect_type_counts(v, repo.as_deref(), scope)
                        .unwrap_or_default(),
                    diagnostic_counts: diagnostic_counts_view(v, &diag_scope),
                    hubs: hub_ranking(v, repo.as_deref(), scope, HUB_LIMIT, 0),
                    graph_shape: graph_shape_view(v, repo.as_deref(), scope, false),
                }),
            )
        }
        Request::TypeClosure(args) => {
            let TypeClosureArgs { name, repo } = args;
            enveloped(
                "type_closure",
                handle.read(move |v| wire::introspect_type_closure(v, &name, repo.as_deref())),
            )
        }
        Request::Hubs(args) => {
            let repo = args.repo;
            let scope = wire::TypeScope::new(resolved_scope(args.scope, &repo).own_only());
            let limit = args.limit.unwrap_or(HUB_LIMIT);
            let offset = args.offset.unwrap_or(0);
            enveloped(
                "hubs",
                handle.read(move |v| hub_ranking(v, repo.as_deref(), scope, limit, offset)),
            )
        }
        Request::GraphShape(args) => {
            let repo = args.repo;
            let scope = wire::TypeScope::new(resolved_scope(args.scope, &repo).own_only());
            let orphan_paths = args.orphan_paths;
            enveloped(
                "graph_shape",
                handle.read(move |v| graph_shape_view(v, repo.as_deref(), scope, orphan_paths)),
            )
        }
        Request::LinkGraph(args) => {
            let repo = args.repo;
            let scope = wire::TypeScope::new(resolved_scope(args.scope, &repo).own_only());
            enveloped(
                "link_graph",
                handle.read(move |v| link_graph_view(v, repo.as_deref(), scope)),
            )
        }
        Request::TypeGraph(args) => {
            let repo = args.repo;
            let scope = wire::TypeScope::new(resolved_scope(args.scope, &repo).own_only());
            let classes = EdgeClasses::from_arg(args.edges);
            enveloped(
                "type_graph",
                handle.read(move |v| type_graph_view(v, repo.as_deref(), scope, classes)),
            )
        }
        Request::Ignores { repo, resolve } => {
            // Capture the member set (name, root) under the held lock, then read
            // each `.auignore` (and, for `resolve`, walk) off the lock and off
            // the runtime workers — the same out-of-band, disk-reading shape
            // `content` uses, since scope config is not held state.
            let members = handle.read(move |v| {
                v.repos
                    .repos()
                    .iter()
                    // The builtin `au-engine` repo is a type source, not a
                    // workspace member (no real root), so it carries no ignores.
                    .filter(|r| !r.builtin)
                    .filter(|r| repo.as_deref().is_none_or(|want| r.name.as_str() == want))
                    .map(|r| (r.name.as_str().to_string(), r.root.clone()))
                    .collect::<Vec<_>>()
            });
            match members {
                Read::NotReady => Response::not_ready(),
                Read::Ready { version, value } => {
                    // A panic in the off-lock walk cannot answer; signal not-ready
                    // (the client's existing retry path) rather than let the
                    // JoinError panic the handler and sever this one connection.
                    match tokio::task::spawn_blocking(move || {
                        wire::ignores_view(&au_parser::RealFileSystem, &value, resolve)
                    })
                    .await
                    {
                        Ok(view) => Response::ready(
                            version,
                            serde_json::to_value(view).expect("view serializes"),
                        ),
                        Err(_) => Response::not_ready(),
                    }
                }
            }
        }
        Request::ResolveMember { path } => {
            let target = abs_arg(handle.root(), &path);
            let cache_root = handle.package_cache_root().map(Path::to_path_buf);
            enveloped(
                "resolve_member",
                handle.read(move |v| wire::resolve_member(v, &target, cache_root.as_deref())),
            )
        }
        Request::DeviceConfig => {
            // The two device-global files sit OUTSIDE every knowledge base, so their
            // bytes are read out-of-band (disk, off the held lock and off the
            // runtime workers), the shape `content` / `ignores` use. Their paths
            // come from the engine's stated `ConfigSource`: a test tempdir
            // (`Dir`), the env default (`User`), or nothing at all (`Empty`).
            // The field-shape check then runs under the lock on the held
            // snapshot, where the builtin `au-engine` graph carries the defs.
            let config = handle.config().clone();
            let repos_path = crate::repo::user_registry_path(&config);
            let workspaces_path = crate::repo::user_workspaces_path(&config);
            let (rp, wp) = (repos_path.clone(), workspaces_path.clone());
            let bytes = tokio::task::spawn_blocking(move || {
                let read = |p: &Option<PathBuf>| p.as_deref().and_then(|p| std::fs::read(p).ok());
                (read(&rp), read(&wp))
            })
            .await
            // The read task only does `std::fs::read().ok()`, so it cannot panic;
            // degrade to no bytes rather than propagate a JoinError and sever the
            // connection.
            .unwrap_or((None, None));
            enveloped(
                "device_config",
                handle.read(move |v| {
                    wire::device_config_view(v, (repos_path, bytes.0), (workspaces_path, bytes.1))
                }),
            )
        }
        Request::Config(args) => config_response(handle, args).await,
    }
}

/// The `config` read: one consumer config file under the scoped channel. Machine
/// scope resolves the `.arsumbris` dir from the engine's `ConfigSource` and the
/// type in the served entry's graph; repo scope resolves under a declared member
/// root (explicit `root`, else the entry) and the type in that member's graph.
/// Path-safety and the reserved `au-engine` owner segment reject a bad segment
/// with an `error` response, an unknown repo-scope `root` likewise.
async fn config_response(handle: &EngineHandle, args: ConfigArgs) -> Response {
    // Path-safety is checked at the verb, independent of scope, so a malformed
    // segment rejects even when the scope has no resolvable base.
    if let Err(e) = crate::repo::check_config_segments(&args.consumer, &args.file) {
        return Response::invalid(e);
    }

    // Resolve the scope's `.arsumbris` dir and the graph the type resolves in,
    // under one lock. A machine scope with no device root yields `None` dir.
    enum Scope {
        Ready(Option<PathBuf>, String),
        BadRoot(String),
    }
    let base = handle.config().base();
    let want_root = args.root.as_deref().map(|r| abs_arg(handle.root(), r));
    let scope = args.scope;
    let resolved = handle.read(move |v| match scope {
        ConfigScope::Machine => {
            let entry = v
                .repos
                .root()
                .map(|r| r.name.as_str().to_string())
                .unwrap_or_default();
            Scope::Ready(base, entry)
        }
        ConfigScope::Repo => {
            let member = match &want_root {
                Some(p) => v.repos.repos().iter().find(|r| &r.root == p),
                None => v.repos.root(),
            };
            match member {
                Some(r) => {
                    Scope::Ready(Some(r.root.join(".arsumbris")), r.name.as_str().to_string())
                }
                None => Scope::BadRoot(format!(
                    "repo scope needs a declared member root; `{}` names none",
                    want_root
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<entry>".to_string())
                )),
            }
        }
    });
    let (dir, scope_repo) = match resolved {
        Read::NotReady => return Response::not_ready(),
        Read::Ready {
            value: Scope::BadRoot(msg),
            ..
        } => return Response::invalid(msg),
        Read::Ready {
            value: Scope::Ready(dir, repo),
            ..
        } => (dir, repo),
    };

    // Machine scope with no device root ($HOME unset): nowhere to author, a
    // null-path view rather than an error.
    let Some(dir) = dir else {
        return enveloped(
            "config",
            handle.read(|_v| wire::ConfigFileView {
                path: None,
                exists: false,
                content: None,
                diagnostics: Vec::new(),
            }),
        );
    };

    // Segments already checked, so this only rebuilds the path; a defensive
    // reject on the impossible error keeps the guarantee at the join site.
    let path = match crate::repo::config_path(&dir, &args.consumer, &args.file) {
        Ok(p) => p,
        Err(e) => return Response::invalid(e),
    };

    // The bytes are read off the runtime workers, off the held lock, then the
    // field-shape check runs under the lock on the held snapshot.
    let read_path = path.clone();
    let bytes = tokio::task::spawn_blocking(move || std::fs::read(&read_path).ok())
        .await
        .unwrap_or(None);
    let type_name = args.type_name;
    enveloped(
        "config",
        handle.read(move |v| wire::config_view(v, &path, bytes, &type_name, &scope_repo)),
    )
}

/// The `instances_of` read, with the optional per-match `instance` and `body`
/// facts.
///
/// Two phases, because `body` reads disk. The held read builds the base match
/// records (each already carrying `member`) and splices the `instance` fact when
/// asked, both pure held state under one lock. The `body` fact then reads disk
/// off the executor, so a per-match file read never blocks the held state or a
/// runtime worker — the same off-lock disk pattern the `content` read uses.
///
/// The `neighborhood` read, two-phase like [`instances_of_response`] so the
/// disk-reading enrichment never runs under the state lock.
///
/// Phase 1, under the lock: the walk, the DTO, and the in-memory `instance`
/// splice (`resolved_view`). Phase 2, off the executor: `content` / `body`, read
/// from the working tree once per unique file path; a block node slices its span
/// from that same read. The kinds validation already ran in the dispatch arm, so
/// `args` arrives validated.
async fn neighborhood_response(
    handle: &EngineHandle,
    seed: PathBuf,
    args: NeighborhoodArgs,
) -> Response {
    let want_content = args.content;
    let want_body = args.body;
    let want_instance = args.instance;
    let kinds: Option<std::collections::BTreeSet<String>> =
        args.kinds.map(|ks| ks.into_iter().collect());
    let direction = args.direction.into();
    let depth = args.depth;
    let scope_own = args.scope.own_only();
    let max_nodes = args.max_nodes.unwrap_or(NEIGHBORHOOD_MAX_NODES);

    // Phase 1: the held read. The walk, the DTO serialized to a mutable value,
    // the in-memory `instance` splice, and the per-node file path the disk phase
    // enriches (None for a block node).
    let read = handle.read(move |v| {
        let params = crate::neighborhood::WalkParams {
            direction,
            depth,
            kinds: kinds.as_ref(),
            scope_own,
            max_nodes,
        };
        let walk = crate::neighborhood::walk(v, &seed, &params);
        let mut payload =
            serde_json::to_value(neighborhood_view(v, &walk)).expect("view serializes");

        // One enrichment site per node, in node order: the file path plus, for a
        // block node, its resolved span (None = the whole file). A block whose
        // id did not resolve has no span, so its slice is null.
        let sites: Vec<EnrichSite> = walk
            .nodes
            .iter()
            .map(|n| EnrichSite {
                path: n.id.path.clone(),
                span: n.block_span,
                is_block: n.id.block_id.is_some(),
            })
            .collect();

        if want_instance {
            let nodes = payload["nodes"].as_array_mut().expect("nodes is an array");
            for (node, site) in nodes.iter_mut().zip(&sites) {
                // A block node has no standalone resolved instance; only a file
                // node carries one (null for a plain note).
                node["instance"] = if site.is_block {
                    serde_json::Value::Null
                } else {
                    serde_json::to_value(resolved_view(v, &site.path)).expect("resolved serializes")
                };
            }
        }
        (payload, sites)
    });

    let (version, mut payload, sites) = match read {
        Read::NotReady => return Response::not_ready(),
        Read::Ready {
            version,
            value: (payload, sites),
        } => {
            // No disk facts requested: phase 1 is the whole answer.
            if !want_content && !want_body {
                return Response::ready(version, serde_json::json!({ "neighborhood": payload }));
            }
            (version, payload, sites)
        }
    };

    // Phase 2: read each unique file ONCE off the executor, deriving `content`,
    // `body`, and (for a path hosting a block) the bytes a span slices from.
    // Paths that host a block node, so their buffer is retained for slicing; a
    // pure-file-node path drops it after deriving content / body.
    let block_paths: std::collections::HashSet<PathBuf> = sites
        .iter()
        .filter(|s| s.span.is_some())
        .map(|s| s.path.clone())
        .collect();
    let unique: Vec<PathBuf> = {
        let mut seen = std::collections::HashSet::new();
        sites
            .iter()
            .filter(|s| seen.insert(s.path.clone()))
            .map(|s| s.path.clone())
            .collect()
    };
    let facts: std::collections::HashMap<PathBuf, FileFacts> =
        tokio::task::spawn_blocking(move || {
            unique
                .into_iter()
                .map(|p| {
                    let keep = block_paths.contains(&p);
                    let facts = read_neighborhood_file(&p, want_content, want_body, keep);
                    (p, facts)
                })
                .collect()
        })
        .await
        // The read task only does fallible IO; degrade to no facts rather than
        // sever the connection on a JoinError.
        .unwrap_or_default();

    let nodes = payload["nodes"].as_array_mut().expect("nodes is an array");
    for (node, site) in nodes.iter_mut().zip(&sites) {
        let file = facts.get(&site.path);
        let (content, body) = match site.span {
            // A block node: slice its span from the file bytes. A block is not a
            // file, so its content carries no hash / commit.
            Some(span) => {
                let slice = file
                    .and_then(|f| f.bytes.as_ref())
                    .and_then(|b| b.get(span.start..span.end))
                    .and_then(|s| std::str::from_utf8(s).ok());
                let content =
                    slice.map(|t| serde_json::json!({ "text": t, "hash": null, "commit": null }));
                (content, slice.map(str::to_string))
            }
            // A file node (or a block with no resolved span → null): the whole
            // file's content / body.
            None if !site.is_block => (
                file.and_then(|f| f.content.clone()),
                file.and_then(|f| f.body.clone()),
            ),
            None => (None, None),
        };
        if want_content {
            node["content"] = serde_json::to_value(content).expect("content serializes");
        }
        if want_body {
            node["body"] = serde_json::to_value(body).expect("body serializes");
        }
    }
    Response::ready(version, serde_json::json!({ "neighborhood": payload }))
}

/// One node's enrichment site: the file it lives in, and (for a block node) the
/// span to slice from that file. `span` is `None` for a file node (the whole
/// file) or a block whose id did not resolve.
struct EnrichSite {
    path: PathBuf,
    span: Option<au_diagnostics::ByteRange>,
    is_block: bool,
}

/// The disk facts read once per unique file: the whole-file `content` value and
/// `body`, plus the raw bytes when a block slice is taken off them.
#[derive(Default)]
struct FileFacts {
    content: Option<serde_json::Value>,
    body: Option<String>,
    bytes: Option<Vec<u8>>,
}

/// Facts key by the match's FILE and are computed ONCE per unique path, then
/// spliced onto every record sharing it: a bare name matching several identities
/// in one file yields several records, and re-resolving or re-reading the file
/// per record would multiply the cost for no new information.
async fn instances_of_response(
    handle: &EngineHandle,
    type_name: String,
    origins: Option<Vec<wire::Origin>>,
    want_instance: bool,
    want_body: bool,
) -> Response {
    // Phase 1: the held read. Base records + `member` + the optional `instance`
    // fact, plus each record's file path for the body phase.
    let read = handle.read(move |v| {
        let matches = wire::introspect_instances_of(v, &type_name, origins.as_deref());
        let mut records: Vec<serde_json::Value> = matches
            .iter()
            .map(|m| serde_json::to_value(m).expect("match serializes"))
            .collect();

        if want_instance {
            let mut by_path: std::collections::HashMap<&str, serde_json::Value> =
                std::collections::HashMap::new();
            for m in &matches {
                by_path.entry(m.path.as_str()).or_insert_with(|| {
                    serde_json::to_value(resolved_view(v, Path::new(&m.path)))
                        .expect("resolved view serializes")
                });
            }
            for (rec, m) in records.iter_mut().zip(&matches) {
                rec["instance"] = by_path[m.path.as_str()].clone();
            }
        }

        let paths: Vec<String> = matches.into_iter().map(|m| m.path).collect();
        (records, paths)
    });

    let (version, mut records, paths) = match read {
        Read::NotReady => return Response::not_ready(),
        Read::Ready {
            version,
            value: (records, paths),
        } => {
            if !want_body {
                return Response::ready(version, serde_json::json!({ "instances_of": records }));
            }
            (version, records, paths)
        }
    };

    // Phase 2: the `body` fact, read from disk off the executor, once per unique
    // path.
    let unique: Vec<PathBuf> = {
        let mut seen = std::collections::HashSet::new();
        paths
            .iter()
            .filter(|p| seen.insert(p.as_str()))
            .map(PathBuf::from)
            .collect()
    };
    let bodies: std::collections::HashMap<PathBuf, Option<String>> =
        tokio::task::spawn_blocking(move || {
            unique
                .into_iter()
                .map(|p| {
                    let b = read_body_at(&p);
                    (p, b)
                })
                .collect()
        })
        .await
        // The read task only does fallible IO, never panics; degrade to no body
        // rather than sever the connection on a JoinError.
        .unwrap_or_default();
    for (rec, path) in records.iter_mut().zip(&paths) {
        // A file that could not be read (or an unterminated one) serializes
        // its body fact as null, never a dropped key.
        let b = bodies.get(Path::new(path)).cloned().flatten();
        rec["body"] = serde_json::to_value(b).expect("body serializes");
    }

    Response::ready(version, serde_json::json!({ "instances_of": records }))
}

/// Read a file's markdown BODY, the prose after its frontmatter. The `body`
/// half of the `content` read's whole-file text, split with the SAME
/// `split_frontmatter` the parser uses, so the two can never disagree on the
/// boundary.
///
/// `None` when the file is unreadable, not UTF-8, or opens an unterminated
/// frontmatter (a structural break the caller's own re-parse rejects). A file
/// with no frontmatter is all body.
fn read_body_at(target: &Path) -> Option<String> {
    let bytes = std::fs::read(target).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    match au_parser::split_frontmatter(&text) {
        Ok(Some(split)) => Some(split.body.to_string()),
        Ok(None) => Some(text),
        Err(_) => None,
    }
}

/// Resolve a request path argument against the root: absolute paths pass
/// through, relative paths join the knowledge base root. Catalog keys are absolute, so
/// either form a consumer sends lands on the same key.
fn abs_arg(root: &Path, arg: &str) -> PathBuf {
    let p = Path::new(arg);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

/// Read a path's content and FNV-1a hash off the held lock, the shape the
/// `content` read returns. The hash rides with the content from one read of the
/// bytes, so a consumer always holds a guard-usable `expected_hash`.
fn read_content_at(target: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(target).ok()?;
    let hash = crate::mutate::hash_hex(crate::ir::ContentHash::of(&bytes));
    // The pin anchor for the returned bytes: HEAD of the repo owning the file,
    // null when the file is not under a git working tree. The bytes are the
    // working tree's (possibly dirty), so a consumer pinning `file*@<commit>`
    // anchors against HEAD, symmetric with a mutation's `result.commit`.
    // HEAD via a direct ref-file read (no per-open git subprocess), the shell
    // fallback covers any state the direct read cannot resolve.
    let commit = enclosing_dir(target).and_then(crate::gitwriter::head_commit_fast);
    let content = String::from_utf8(bytes).ok()?;
    // `text`, not `content`: the payload key is the read's own name, so a
    // `content` field inside it would make `result.content.content` the way to
    // the source string — the same silent-`undefined` trap one level down. The
    // envelope rule fixes a collision on the INSIDE. See `WIRE.md`.
    Some(serde_json::json!({ "text": content, "hash": hash, "commit": commit }))
}

/// Read one file ONCE and derive every requested `neighborhood` fact from the
/// single buffer: the whole-file `content` value, the `body`, and the raw bytes
/// a block node slices its span from. The `content` / `body` results match
/// [`read_content_at`] / [`read_body_at`] exactly, but a file feeding both plus
/// a block slice is read once rather than up to three times.
///
/// `keep_bytes` retains the buffer only for a path some block node lives in; a
/// pure-file-node path drops it.
fn read_neighborhood_file(
    target: &Path,
    want_content: bool,
    want_body: bool,
    keep_bytes: bool,
) -> FileFacts {
    let bytes = match std::fs::read(target) {
        Ok(b) => b,
        Err(_) => return FileFacts::default(),
    };
    // One UTF-8 view drives both text facts; a non-UTF-8 file yields neither,
    // matching the per-fact readers' `String::from_utf8(...).ok()?`.
    let text = std::str::from_utf8(&bytes).ok();
    let content = (want_content && text.is_some()).then(|| {
        let hash = crate::mutate::hash_hex(crate::ir::ContentHash::of(&bytes));
        let commit = enclosing_dir(target).and_then(crate::gitwriter::head_commit_fast);
        serde_json::json!({ "text": text.unwrap(), "hash": hash, "commit": commit })
    });
    let body = if want_body {
        // Same split as `read_body_at`: an unterminated frontmatter yields None.
        text.and_then(|t| match au_parser::split_frontmatter(t) {
            Ok(Some(split)) => Some(split.body.to_string()),
            Ok(None) => Some(t.to_string()),
            Err(_) => None,
        })
    } else {
        None
    };
    FileFacts {
        content,
        body,
        bytes: keep_bytes.then_some(bytes),
    }
}

/// The directory a `git -C` query runs in, the file's parent.
fn enclosing_dir(target: &Path) -> Option<&Path> {
    target.parent()
}

/// Turn a [`Read`] into a response carrying the uniform result envelope:
/// `result` is an object holding the payload under the READ'S OWN NAME, so
/// `result[verb]` always reaches it. See `WIRE.md`, "The result envelope".
///
/// A nullable read wraps its null (`{ "type": null }`), so "not found" is
/// reached exactly like a hit — the shape never changes under the consumer.
///
/// `verb` is the wire name, which is why it is passed rather than derived: the
/// `Request` variant name and the wire name can differ, and the wire name is
/// the contract.
fn enveloped<T: Serialize>(verb: &'static str, read: Read<T>) -> Response {
    match read {
        Read::Ready { version, value } => Response::ready(
            version,
            serde_json::json!({ verb: serde_json::to_value(value).expect("result serializes") }),
        ),
        Read::NotReady => Response::not_ready(),
    }
}

/// Turn a [`Read`] of a serializable value into a response.
fn to_response<T: Serialize>(read: Read<T>) -> Response {
    match read {
        Read::Ready { version, value } => Response::ready(
            version,
            serde_json::to_value(value).expect("result serializes"),
        ),
        Read::NotReady => Response::not_ready(),
    }
}

fn resolved_view(kb: &KnowledgeBase, target: &Path) -> Option<ResolvedView> {
    let resolved = kb.instances.get(target)?;
    let Some(FileParse::Instance {
        instance: Some(inst),
        ..
    }) = kb.file_parse(target)
    else {
        return None;
    };

    // Authored form, so a `::repo` claim stays qualified, matching `instances_of`
    // / `instances`. It was served bare here.
    let claim = inst.type_claim.iter().map(|c| c.authored()).collect();
    // Owner-relative closure (WIRE §272), the same projection `instances` uses, so
    // a cross-repo ancestor keeps its `::repo` here too, rendered relative to the
    // instance's own graph. Empty when the claim did not resolve (no shape to fold).
    let resolution = kb
        .repos
        .repo_of(target)
        .and_then(|r| kb.resolution_graphs.of(&r.name));
    let own_graph = kb.graph_for_path(target);
    let closure = resolved
        .effective_shape
        .as_ref()
        .map(|s| wire::instance_closure_authored(own_graph, resolution, &inst.type_claim, s))
        .unwrap_or_default();
    let candidates = wire::candidates_for(kb, target)
        .iter()
        .map(|c| CandidateDto {
            type_name: c.type_name.0.clone(),
            inline_path: c.scope.inline_path.clone(),
        })
        .collect();
    // The full value layer, with provenance. Computed for any parsed instance;
    // an unresolved claim still carries frontmatter and body contributions.
    let layer = wire::instance_value_layer(kb, target);
    let (effective_values, section_presence, body_events) = match layer {
        Some(l) => (l.effective_values, l.section_presence, l.body_events),
        None => (Vec::new(), None, None),
    };
    let diagnostics = kb.diagnostics_for_file(target).cloned().collect();

    Some(ResolvedView {
        resolved: resolved.effective_shape.is_some(),
        claim,
        closure,
        candidates,
        effective_values,
        section_presence,
        body_events,
        record_block_ids: wire::record_block_ids_of(kb, target, inst, kb.line_index(target)),
        diagnostics,
    })
}

fn backlinks_view(kb: &KnowledgeBase, target: &Path) -> Vec<BacklinkDto> {
    let target_repo = kb.repos.repo_of(target).map(|r| r.name.clone());
    kb.backlinks(target)
        .iter()
        .map(|b| BacklinkDto {
            source: b.source.display().to_string(),
            // Carry the source repo only when it differs from the target's, the
            // cross-repo inbound edge.
            repo: kb
                .repos
                .repo_of(&b.source)
                .map(|r| r.name.clone())
                .filter(|src| Some(src) != target_repo.as_ref())
                .map(|src| src.as_str().to_string()),
            slot: b.slot.clone(),
            surface: match b.surface {
                RefSurface::Frontmatter => "frontmatter",
                RefSurface::Body => "body",
                RefSurface::Docstring => "docstring",
            }
            .to_string(),
            kind: crate::backlinks::coarse_edge_kind(b.surface, b.slot.as_deref()),
            span_start: b.span.start,
            span_end: b.span.end,
            // The span sits in the referencing source file, not the target.
            line_col: kb
                .line_index(&b.source)
                .map(|idx| idx.line_col_range(b.span)),
            block_id: b.block_id.clone(),
            source_block_id: b.source_block_id.clone(),
        })
        .collect()
}

/// Project a walked [`crate::neighborhood::Neighborhood`] into the wire DTO.
/// The walk is the analysis; this only renders. An edge span indexes its
/// `from` (referrer) file, so line/col comes from that file's index.
fn neighborhood_view(kb: &KnowledgeBase, n: &crate::neighborhood::Neighborhood) -> NeighborhoodDto {
    let node_ref = |id: &crate::neighborhood::NodeId| NodeRefDto {
        path: id.path.display().to_string(),
        block_id: id.block_id.clone(),
    };
    NeighborhoodDto {
        nodes: n
            .nodes
            .iter()
            .map(|w| NeighborhoodNodeDto {
                path: w.id.path.display().to_string(),
                block_id: w.id.block_id.clone(),
                depth: w.depth,
                repo: w.repo.as_ref().map(|r| r.as_str().to_string()),
                file_kind: w.file_kind,
                bytes: w.byte_len,
                body_bytes: w.body_bytes,
            })
            .collect(),
        edges: n
            .edges
            .iter()
            .map(|e| {
                // The rich fragments live on the parsed link, present only for
                // an outbound-discovered edge. An inbound edge carries none.
                let (repo, commit, anchor) = match &e.link {
                    Some(l) => (l.repo.clone(), l.commit.clone(), l.anchor.clone()),
                    None => (None, None, None),
                };
                NeighborhoodEdgeDto {
                    from: node_ref(&e.from),
                    to: e.to.as_ref().map(node_ref),
                    kind: e.kind,
                    surface: match e.surface {
                        crate::backlinks::RefSurface::Frontmatter => "frontmatter",
                        crate::backlinks::RefSurface::Body => "body",
                        crate::backlinks::RefSurface::Docstring => "docstring",
                    },
                    span_start: e.span.start,
                    span_end: e.span.end,
                    line_col: kb
                        .line_index(&e.from.path)
                        .map(|idx| idx.line_col_range(e.span)),
                    field: e.field.clone(),
                    source_block_id: e.source_block_id.clone(),
                    block_id: e.target_block_id.clone(),
                    repo,
                    commit,
                    anchor,
                }
            })
            .collect(),
        truncated: n.truncated,
        truncated_at_depth: n.truncated_at_depth,
        dropped: n
            .dropped
            .iter()
            .map(|d| DroppedNodeDto {
                path: d.id.path.display().to_string(),
                block_id: d.id.block_id.clone(),
                depth: d.depth,
                repo: d.repo.as_ref().map(|r| r.as_str().to_string()),
            })
            .collect(),
    }
}

/// EVERY outgoing wikilink edge of a file, frontmatter and body, each
/// classified by kind. Frontmatter edges first (declaration order), then body
/// edges in source order.
///
/// One read answers "what does this connect to?" completely. Before this it was
/// scattered: body-only here, typed frontmatter references only via the
/// `instance` read, and a `[[...]]` inside a frontmatter string nowhere in the
/// forward direction at all.
///
/// The traversal is SHARED with the backlink index
/// ([`crate::backlinks::walk_edges`]), so the two directions cannot disagree
/// about which values are scanned or how a target resolves. This read adds only
/// the classification, which needs the effective shape the walk deliberately
/// does not see.
fn outgoing_view(kb: &KnowledgeBase, target: &Path) -> Vec<OutgoingRefDto> {
    let Some(parse) = kb.file_parse(target) else {
        return Vec::new();
    };
    // The slot gate, three-way. A plain Note is DEFINITIVELY untyped (no `type:`
    // claim at all), so no slot can admit a reference, which is knowledge, not
    // uncertainty. A typed instance carries a shape, unless its claim did not
    // resolve or its repo's vocabulary aborted (`build.rs` skips those when
    // populating the resolved layer) — only then is the slot genuinely unknown.
    let gate = match parse {
        FileParse::Note { .. } => SlotGate::Untyped,
        _ => match kb
            .instances
            .get(target)
            .and_then(|r| r.effective_shape.as_ref())
        {
            Some(shape) => SlotGate::Shape(shape),
            None => SlotGate::Unknown,
        },
    };

    crate::backlinks::walk_edges(target, parse, &kb.indexes, &kb.repos, &kb.workspaces)
        .into_iter()
        .map(|e| {
            let kind = classify_edge(&e, gate);
            let w = e.link;
            OutgoingRefDto {
                span: wire::SpanRange::new(e.span.start, e.span.end).located(kb.line_index(target)),
                target: w.target,
                resolved: e.resolved.map(|p| p.display().to_string()),
                repo: w.repo,
                commit: w.commit,
                anchor: w.anchor,
                block_id: w.block_id,
                field: e.slot,
                source_block_id: e.source_block_id,
                surface: match e.surface {
                    crate::backlinks::RefSurface::Frontmatter => "frontmatter",
                    crate::backlinks::RefSurface::Body => "body",
                    crate::backlinks::RefSurface::Docstring => "docstring",
                },
                kind,
            }
        })
        .collect()
}

/// The reverse-by-target inert-pin fold: every pin naming `target_name` whose
/// source file contains an instance of `source_type`. See [`PinRecordDto`] and
/// [[spec - pinned references - a recorded resolved edge with an immutable past
/// and an on-demand forward trace]].
///
/// Scope, then fold. `source_type` scopes the candidate FILE set via
/// `wire::instance_files_of` — the files containing an instance of that type (a
/// file-level claim or a nested inline record), already deduped to one entry per
/// file. It shares the identity rule with `instances_of` but skips the match
/// projection a path-only caller would discard. For each such file, `walk_edges`
/// yields every outbound reference — the same walk `references_out` uses, so
/// nested-record pins carry their `source_block_id` — and this keeps the
/// commit-bearing ones naming the target.
///
/// No time awareness and no live resolution: the match is on the pin's RECORDED
/// target string, so a since-reused name returns every pin naming it. The
/// consumer windows by rename time to exclude a since-reused name.
fn pins_view(kb: &KnowledgeBase, target_name: &str, source_type: &str) -> Vec<PinRecordDto> {
    let mut out = Vec::new();
    for source in wire::instance_files_of(kb, source_type) {
        let Some(parse) = kb.file_parse(&source) else {
            continue;
        };
        for e in
            crate::backlinks::walk_edges(&source, parse, &kb.indexes, &kb.repos, &kb.workspaces)
        {
            // Only inert pins, and only those naming the requested target. An
            // empty-target commit-referent (`[[::@sha]]`) names no file, so it
            // never matches a named target and is excluded here for free.
            if !e.link.is_inert_pin() || e.link.target != target_name {
                continue;
            }
            let w = e.link;
            out.push(PinRecordDto {
                source: source.display().to_string(),
                source_block_id: e.source_block_id,
                span: wire::SpanRange::new(e.span.start, e.span.end)
                    .located(kb.line_index(&source)),
                slot: e.slot,
                surface: match e.surface {
                    crate::backlinks::RefSurface::Frontmatter => "frontmatter",
                    crate::backlinks::RefSurface::Body => "body",
                    crate::backlinks::RefSurface::Docstring => "docstring",
                },
                target: w.target,
                repo: w.repo,
                // A pin is commit-bearing by definition (`is_inert_pin`), so the
                // commit is present; default only defends against an impossible
                // shape rather than panicking on the socket path.
                commit: w.commit.unwrap_or_default(),
                block_id: w.block_id,
            });
        }
    }
    out
}

/// The slot-classification gate for a source file's frontmatter edges. Three
/// states, because "no shape" has three distinct causes and only one is
/// genuinely `unknown`.
#[derive(Clone, Copy)]
enum SlotGate<'a> {
    /// A typed instance with a resolved effective shape: consult the slot.
    Shape(&'a au_core::EffectiveShape),
    /// A plain note, no `type:` claim at all. DEFINITIVELY untyped, so no slot
    /// can admit a reference — a whole-value frontmatter link is an untyped
    /// pointer, not an "we could not tell". Knowledge, not uncertainty.
    Untyped,
    /// A typed instance whose shape is unavailable: its `type:` claim did not
    /// resolve, or its repo's vocabulary aborted. The slot is genuinely
    /// undecidable, so a whole-value frontmatter link is `unknown`.
    Unknown,
}

/// Which KIND of edge this is. See [`OutgoingRefDto::kind`] for what each means.
///
/// Only the frontmatter pair needs the slot. A body edge is settled by the
/// link's own syntax, and an EMBEDDED frontmatter link is settled by
/// [[type reference::au-type-system]] at the value level, so neither can be `unknown`.
fn classify_edge(edge: &crate::backlinks::SourceEdge, gate: SlotGate) -> &'static str {
    // A commit-referent (`[[::@sha]]` / `[[::repo@sha]]`) names a COMMIT, not a
    // file. It resolves to nothing but is NOT dangling, so it carries its own
    // kind, and a consumer reading `resolved: null` does not misread it as a
    // broken link. Settled by the link's own syntax, surface-independent.
    if edge.link.is_commit_referent() {
        return "commit-referent";
    }
    if edge.surface == crate::backlinks::RefSurface::Body {
        return if edge.link.field.is_some() {
            "contributing"
        } else {
            "navigational"
        };
    }
    // A link inside a longer string is part of that string, never a reference,
    // whatever the slot admits. Decidable with no type information.
    if !edge.whole_value {
        return "field-string-wikilink";
    }
    // A whole-value frontmatter link. Only a resolved shape can promote it to a
    // `field-reference`; a definitively-untyped note demotes it to an untyped
    // pointer; a genuinely-undecidable slot is `unknown`.
    let shape = match gate {
        SlotGate::Shape(shape) => shape,
        SlotGate::Untyped => return "field-string-wikilink",
        SlotGate::Unknown => return "unknown",
    };
    let Some(key) = edge.slot.as_deref() else {
        return "unknown";
    };
    match shape.get(&au_core::FieldName(key.to_string())) {
        Some(origin) => {
            if origin
                .canonical_decl()
                .parsed_shape
                .as_ref()
                .is_ok_and(au_grammar::slot_admits_reference)
            {
                "field-reference"
            } else {
                "field-string-wikilink"
            }
        }
        // In the shape but absent from it: an EXTRA field, which has no slot at
        // all. That is knowledge, not uncertainty, so it is not `unknown`.
        None => "field-string-wikilink",
    }
}

/// A file's typed highlight tokens, an ordered `(range, kind)` stream.
/// `None` when the path is not a file the build holds.
///
/// Two layers compose. The body-surface kinds (wikilink, block-id marker,
/// anchor) come from the markdown body, so they serve notes and markdown
/// instances. The value-layer kinds (field-value, type-claim, wikilink-
/// valued frontmatter, `^:` record block-ids) come from a parsed instance,
/// so they serve typed instances including pure-YAML ones with no body.
/// Tokens are sorted by `range.start`.
fn semantic_tokens_view(kb: &KnowledgeBase, target: &Path) -> Option<Vec<SemanticToken>> {
    let parse = kb.file_parse(target)?;
    let mut tokens = Vec::new();

    // A type-def file emits the shape layer.
    if let FileParse::TypeDef {
        type_def: Some(def),
        ..
    } = parse
    {
        let lines = kb.line_index(target);
        type_def_tokens(def, lines, &mut tokens);
        tokens.sort_by_key(|t| t.range.start);
        return Some(tokens);
    }

    let lines = kb.line_index(target);
    let inst = match parse {
        FileParse::Instance {
            instance: Some(inst),
            ..
        } => Some(inst),
        _ => None,
    };

    if let Some((body, body_offset)) = parse.markdown_body() {
        body_tokens(kb, target, inst, body, body_offset, lines, &mut tokens);
    }
    if let Some(inst) = inst {
        frontmatter_tokens(kb, target, inst, lines, &mut tokens);
    }

    tokens.sort_by_key(|t| t.range.start);
    Some(tokens)
}

/// The type-def shape layer: `type-claim` for parent and `meta:` claims,
/// `type-ref` for sealed branches, and per field a `field-shape` container
/// (carrying the parsed `WireShape`) enclosing `type-ref` / `shape-builtin` /
/// `enum-member` leaves. `lines` is the file's index.
/// A claim / parent / meta type-name for a semantic token, carrying its `::repo`
/// qualifier so a consumer can tell a peer type from an own one. Parity with the
/// other wire projections (`InstanceMatch.claim`, meta / body-use targets), which
/// serve the authored qualified form.
fn qualified_token_name(base: &au_core::TypeName, repo: Option<&str>) -> String {
    match repo {
        Some(r) => format!("{}::{}", base.as_str(), r),
        None => base.as_str().to_string(),
    }
}

fn type_def_tokens(
    def: &au_core::TypeDef,
    lines: Option<&au_diagnostics::LineIndex>,
    out: &mut Vec<SemanticToken>,
) {
    let emit = |out: &mut Vec<SemanticToken>, span: au_diagnostics::ByteRange, kind| {
        out.push(SemanticToken {
            range: wire::SpanRange::new(span.start, span.end).located(lines),
            kind,
        });
    };

    // Parent claims and meta-block `type:` are type-claims, like an instance's.
    for parent in &def.parents {
        emit(
            out,
            parent.span,
            SemanticTokenKind::TypeClaim {
                name: qualified_token_name(&parent.name, parent.repo.as_deref()),
            },
        );
    }
    if let Some(blocks) = &def.meta_blocks {
        for block in blocks {
            emit(
                out,
                block.type_name_span,
                SemanticTokenKind::TypeClaim {
                    name: qualified_token_name(&block.type_name, block.repo.as_deref()),
                },
            );
        }
    }

    // Sealed branches are type-def name references.
    for branch in &def.sealed {
        emit(
            out,
            branch.span,
            SemanticTokenKind::TypeRef {
                name: branch.name.as_str().to_string(),
            },
        );
    }

    // Each field: a field-shape container plus its decomposed leaves.
    for field in &def.fields {
        let value_type = match &field.parsed_shape {
            Ok(shape) => Some(wire::WireShape::from(shape)),
            Err(_) => None,
        };
        emit(
            out,
            field.shape_span,
            SemanticTokenKind::FieldShape {
                field: field.name.as_str().to_string(),
                value_type,
            },
        );

        // Leaf name / keyword / literal spans. They are relative to
        // `raw_shape`, which is the normalized scalar value. It maps linearly
        // onto `shape_span` only when byte-identical to the source slice, which
        // holds for unquoted scalars and flow enums but not for a quoted shape
        // (the quotes shift every offset). Lengths differing flags the
        // misaligned case; emit the container only, never a wrong leaf span.
        let shape_len = field.shape_span.end - field.shape_span.start;
        if field.raw_shape.len() != shape_len {
            continue;
        }
        let base = field.shape_span.start;
        let (_shape, spans) = parse_shape_spanned(&field.raw_shape);
        for sp in spans {
            let text = field.raw_shape[sp.range.start..sp.range.end].to_string();
            let abs = au_diagnostics::ByteRange::new(base + sp.range.start, base + sp.range.end);
            let kind = match sp.role {
                ShapeSpanRole::TypeName => SemanticTokenKind::TypeRef { name: text },
                ShapeSpanRole::Builtin => SemanticTokenKind::ShapeBuiltin { name: text },
                ShapeSpanRole::EnumMember => SemanticTokenKind::EnumMember { value: text },
            };
            emit(out, abs, kind);
        }
    }
}

/// The markdown-body kinds: wikilinks (resolved / broken), bare `^id`
/// block-id markers, headings (anchors), and fenced blocks. A fence's
/// trailing `^id` is a block-id; a typed `[:field]` fence on a typed
/// instance also yields a `typed-block` container plus its inner fields.
fn body_tokens(
    kb: &KnowledgeBase,
    target: &Path,
    inst: Option<&au_core::Instance>,
    body: &str,
    body_offset: usize,
    lines: Option<&au_diagnostics::LineIndex>,
    out: &mut Vec<SemanticToken>,
) {
    for ev in scan_body(body) {
        match ev {
            BodyEvent::Wikilink { raw, span } => {
                let Ok(w) = parse_wikilink_inner(raw) else {
                    continue;
                };
                out.push(SemanticToken {
                    range: span_in_body(span, body_offset, lines),
                    kind: wikilink_kind(kb, target, &w),
                });
            }
            BodyEvent::BlockIdMarker { id, span } => out.push(SemanticToken {
                range: span_in_body(span, body_offset, lines),
                kind: SemanticTokenKind::BlockId { id: id.to_string() },
            }),
            BodyEvent::Heading { text, span, .. } => out.push(SemanticToken {
                range: span_in_body(span, body_offset, lines),
                kind: SemanticTokenKind::Anchor {
                    text: text.to_string(),
                },
            }),
            BodyEvent::FencedBlock {
                info,
                body: fence_body,
                span,
                trailing_block_id,
            } => fenced_block_tokens(
                kb,
                target,
                inst,
                body,
                body_offset,
                info,
                fence_body,
                span,
                trailing_block_id,
                lines,
                out,
            ),
            _ => {}
        }
    }
}

/// A fenced block's tokens. The trailing `^id` is a block-id on any fence. A
/// typed `[:field]` fence on a typed instance also emits a `typed-block`
/// container, then its inner fields typed against the fence field's element
/// type, full parity with frontmatter.
#[allow(clippy::too_many_arguments)]
fn fenced_block_tokens(
    kb: &KnowledgeBase,
    target: &Path,
    inst: Option<&au_core::Instance>,
    body: &str,
    body_offset: usize,
    info: &str,
    fence_body: &str,
    span: au_diagnostics::ByteRange,
    trailing_block_id: Option<&str>,
    lines: Option<&au_diagnostics::LineIndex>,
    out: &mut Vec<SemanticToken>,
) {
    if let Some(id) = trailing_block_id {
        // The id text sits right after the `^`; widen the span by one to
        // cover the marker.
        let at = offset_in(body, id);
        let marker = au_diagnostics::ByteRange::new(at - 1, at + id.len());
        out.push(SemanticToken {
            range: span_in_body(marker, body_offset, lines),
            kind: SemanticTokenKind::BlockId { id: id.to_string() },
        });
    }

    // The typed-block container and its inner fields are value-layer, so a
    // typed instance only. A note's fence yields no typed-block.
    let (Some(field), Some(_inst)) = (extract_field_marker(info), inst) else {
        return;
    };
    out.push(SemanticToken {
        range: span_in_body(span, body_offset, lines),
        kind: SemanticTokenKind::TypedBlock {
            field: field.to_string(),
        },
    });

    // Parse the fence body into an inline record with file-absolute spans,
    // then type its fields against the fence field's element type, reusing
    // the value-layer walk. The fence body's file offset is its position in
    // the markdown body plus the body's own offset.
    let fence_offset = body_offset + offset_in(body, fence_body);
    let Some((inline, _)) = au_core::parse_block_record(target, fence_body, fence_offset) else {
        return;
    };
    let element_shape = kb
        .instances
        .get(target)
        .and_then(|r| r.effective_shape.as_ref())
        .and_then(|es| field_shape(es, field))
        .map(|shape| match shape {
            Shape::List { inner, .. } => inner.as_ref(),
            other => other,
        });
    let value = au_core::InstanceValue::Mapping(inline);
    // A mapping carries no links of its own; its fields carry theirs.
    walk_value_tokens(
        kb,
        target,
        kb.graph_for_path(target),
        &value,
        span,
        &[],
        field,
        element_shape,
        lines,
        out,
    );
}

/// The byte offset of a sub-slice within its parent string. Both must share
/// the same backing buffer, as `scan_body`'s slices do.
fn offset_in(parent: &str, child: &str) -> usize {
    child.as_ptr() as usize - parent.as_ptr() as usize
}

/// The frontmatter value-layer kinds: the `type:` claim, each scalar value
/// (typed via the field's resolved shape, descended to the leaf), wikilink-
/// valued fields, and `^:` inline-record block-ids.
fn frontmatter_tokens(
    kb: &KnowledgeBase,
    target: &Path,
    inst: &au_core::Instance,
    lines: Option<&au_diagnostics::LineIndex>,
    out: &mut Vec<SemanticToken>,
) {
    for claim in inst.type_claim.iter() {
        out.push(SemanticToken {
            range: located(claim.span, lines),
            kind: SemanticTokenKind::TypeClaim {
                name: qualified_token_name(&claim.name, claim.repo.as_deref()),
            },
        });
    }

    let graph = kb.graph_for_path(target);
    let shape_of = kb
        .instances
        .get(target)
        .and_then(|r| r.effective_shape.as_ref());
    for field in &inst.fields {
        let shape = shape_of.and_then(|es| field_shape(es, &field.key));
        walk_value_tokens(
            kb,
            target,
            graph,
            &field.value,
            field.value_span,
            &field.nav_links,
            &field.key,
            shape,
            lines,
            out,
        );
    }

    // Block-id highlight tokens use only the record's SPAN, which is
    // shape-independent, so the own-graph walk suffices (no owner-relative context).
    for (id, record) in au_core::collect_record_targets(graph, None, None, "", inst) {
        out.push(SemanticToken {
            range: located(record.span, lines),
            kind: SemanticTokenKind::BlockId { id },
        });
    }
}

/// Walk one frontmatter value against its shape, emitting a token per scalar
/// leaf. A wikilink-valued string is a wikilink token, not a field-value
/// token. A sequence descends with the list's element shape; a nested inline
/// record recurses (its field types are not resolved in this pass, so its
/// leaves carry a null `value_type`) and contributes its own type claim.
#[allow(clippy::too_many_arguments)]
fn walk_value_tokens(
    kb: &KnowledgeBase,
    source: &Path,
    graph: &au_core::TypeGraph,
    value: &au_core::InstanceValue,
    span: au_diagnostics::ByteRange,
    nav_links: &[au_core::NavLink],
    field: &str,
    shape: Option<&Shape>,
    lines: Option<&au_diagnostics::LineIndex>,
    out: &mut Vec<SemanticToken>,
) {
    use au_core::InstanceValue as V;
    match value {
        V::String(s) => {
            if let Ok(w) = parse_wikilink(s) {
                // The whole value is one wikilink: a single wikilink token
                // over the value span, the validated-or-navigational case.
                out.push(SemanticToken {
                    range: located(span, lines),
                    kind: wikilink_kind(kb, source, &w),
                });
            } else if nav_links.is_empty() {
                out.push(field_value_token(span, field, shape, "String", lines));
            } else {
                // A string with embedded links: a wikilink token per link, a
                // field-value token for each surrounding text run.
                emit_mixed_string_tokens(kb, source, span, nav_links, field, shape, lines, out);
            }
        }
        V::Integer(_) | V::Float(_) => {
            out.push(field_value_token(span, field, shape, "Number", lines));
        }
        V::Boolean(_) => {
            out.push(field_value_token(span, field, shape, "Boolean", lines));
        }
        V::Null | V::NotYetSupported => {}
        V::Sequence(elems) => {
            let inner = match shape {
                Some(Shape::List { inner, .. }) => Some(inner.as_ref()),
                _ => None,
            };
            for e in elems {
                walk_value_tokens(
                    kb,
                    source,
                    graph,
                    &e.value,
                    e.span,
                    &e.nav_links,
                    field,
                    inner,
                    lines,
                    out,
                );
            }
        }
        V::Mapping(inline) => {
            for claim in inline.type_claim.iter().flat_map(|tc| tc.iter()) {
                out.push(SemanticToken {
                    range: located(claim.span, lines),
                    kind: SemanticTokenKind::TypeClaim {
                        name: qualified_token_name(&claim.name, claim.repo.as_deref()),
                    },
                });
            }
            // The record's type, from its explicit claim or the type the slot
            // pins. Either is local: the slot shape or the inline `type:`
            // names the type-def, so its field shapes are a graph lookup, no
            // candidate scan. A record with neither stays untyped (null), the
            // signal for a consumer to run the candidate scan itself.
            let record_shape = record_effective_shape(graph, inline, shape);
            for nested in &inline.fields {
                let nested_slot = record_shape
                    .as_ref()
                    .and_then(|s| field_shape(s, &nested.key));
                walk_value_tokens(
                    kb,
                    source,
                    graph,
                    &nested.value,
                    nested.value_span,
                    &nested.nav_links,
                    &nested.key,
                    nested_slot,
                    lines,
                    out,
                );
            }
        }
    }
}

/// A `field-value` token over a scalar. The type is the declared shape when
/// the engine knows it; otherwise it is inferred from the YAML literal
/// (`literal`, a primitive name) so an extra or untyped-record field still
/// colors by type.
fn field_value_token(
    span: au_diagnostics::ByteRange,
    field: &str,
    shape: Option<&Shape>,
    literal: &'static str,
    lines: Option<&au_diagnostics::LineIndex>,
) -> SemanticToken {
    let value_type = match shape {
        Some(s) => wire::WireShape::from(s),
        None => wire::WireShape::Primitive { name: literal },
    };
    SemanticToken {
        range: located(span, lines),
        kind: SemanticTokenKind::FieldValue {
            field: field.to_string(),
            value_type,
        },
    }
}

/// Tokenize a string value that carries embedded navigational links: a
/// wikilink token per link, a `field-value` token over each surrounding text
/// run. Links are parse-ordered and non-overlapping, so a single left-to-
/// right sweep over the value span covers it without gaps or overlaps.
fn emit_mixed_string_tokens(
    kb: &KnowledgeBase,
    source: &Path,
    span: au_diagnostics::ByteRange,
    nav_links: &[au_core::NavLink],
    field: &str,
    shape: Option<&Shape>,
    lines: Option<&au_diagnostics::LineIndex>,
    out: &mut Vec<SemanticToken>,
) {
    let mut cursor = span.start;
    for nl in nav_links {
        if nl.span.start > cursor {
            // The text run before this link.
            let gap = au_diagnostics::ByteRange::new(cursor, nl.span.start);
            out.push(field_value_token(gap, field, shape, "String", lines));
        }
        let kind = match parse_wikilink_inner(&nl.raw) {
            Ok(w) => wikilink_kind(kb, source, &w),
            Err(_) => {
                cursor = nl.span.end;
                continue;
            }
        };
        out.push(SemanticToken {
            range: located(nl.span, lines),
            kind,
        });
        cursor = nl.span.end;
    }
    if cursor < span.end {
        let tail = au_diagnostics::ByteRange::new(cursor, span.end);
        out.push(field_value_token(tail, field, shape, "String", lines));
    }
}

/// The effective shape of an inline record, for typing its nested fields.
/// The record's type is its explicit `type:` claim, else the single type a
/// `name` / `name&` slot pins. `None` when neither names a type (a
/// compound / sealed slot without an explicit claim, already an
/// `inline-value-missing-type` diagnostic) — that leaves the nested leaves
/// untyped, the consumer's hook to run a candidate scan.
fn record_effective_shape(
    graph: &au_core::TypeGraph,
    inline: &au_core::InlineValue,
    slot: Option<&Shape>,
) -> Option<au_core::EffectiveShape> {
    let names: Vec<au_core::TypeName> = if let Some(claim) = &inline.type_claim {
        claim.iter().map(|c| c.name.clone()).collect()
    } else {
        match slot.and_then(pinned_name) {
            Some(name) => vec![au_core::TypeName(name.to_string())],
            None => return None,
        }
    };
    au_core::effective_shape(graph, &synth_claim(&names)).ok()
}

/// The single type-def name a `name` / `name&` slot pins onto a claim-less
/// inline record. Compound / primitive / list slots pin nothing.
fn pinned_name(shape: &Shape) -> Option<&str> {
    match shape {
        Shape::Record(name) | Shape::InlineOrReference(name) => Some(name.as_str()),
        _ => None,
    }
}

/// A zero-span `TypeClaim` over resolved names, the input to `effective_shape`.
fn synth_claim(names: &[au_core::TypeName]) -> au_core::TypeClaim {
    let items: Vec<au_core::TypeNameClaim> = names
        .iter()
        .map(|n| au_core::TypeNameClaim::own(n.clone(), au_diagnostics::ByteRange::new(0, 0)))
        .collect();
    match &items[..] {
        [one] => au_core::TypeClaim::Bare(one.clone()),
        _ => au_core::TypeClaim::List {
            items,
            value_span: au_diagnostics::ByteRange::new(0, 0),
        },
    }
}

/// A field's canonical declared shape within an effective shape, by bare key.
fn field_shape<'a>(shape: &'a au_core::EffectiveShape, key: &str) -> Option<&'a Shape> {
    shape
        .get(&au_core::FieldName(key.to_string()))
        .and_then(|origin| origin.canonical_decl().parsed_shape.as_ref().ok())
}

/// Resolve a parsed wikilink to its target file, scoped to the `origin` file
/// the link appears in. A `::repo`-qualified link resolves cross-repo into the
/// named repo; an unqualified link resolves repo-local against `origin`'s own
/// index. The single resolution path every reference read shares, so
/// `references_out`, `semantic_tokens`, and the navigation reads agree on what
/// a link points at.
fn resolve_wikilink(
    kb: &KnowledgeBase,
    origin: &Path,
    target: &str,
    repo: Option<&str>,
) -> Option<PathBuf> {
    match repo {
        Some(repo_q) => match crate::crossref::resolve_cross_repo(
            &kb.repos,
            &kb.indexes,
            &kb.workspaces,
            origin,
            repo_q,
            target,
        ) {
            crate::crossref::CrossRepoRef::Resolved(p) => Some(p),
            _ => None,
        },
        None => kb.index_for_path(origin).resolve(target).ok(),
    }
}

/// Resolve a wikilink to a resolved-or-broken token kind. An unqualified link
/// resolves against the source file's repo index (repo-local); a `::repo` link
/// resolves into the named repo, the same cross-repo resolution validation and
/// `references_out` use.
fn wikilink_kind(
    kb: &KnowledgeBase,
    source: &Path,
    w: &au_references::WikilinkRef,
) -> SemanticTokenKind {
    // An inert pin (any `@sha`: a named pin `[[file::@sha]]` or the empty-target
    // commit-referent `[[::@sha]]`) is a coordinate into an immutable past — never
    // re-resolved live, never dangling. So it is its own kind, neither resolved nor
    // broken, over the same `is_inert_pin` predicate the backlink index and navigation
    // reads use (all treat a pin as forming no live edge). NOTE this folds every inert
    // pin into one kind, unlike `references_out`, which reserves the `commit-referent`
    // kind for the empty-target case and leaves a named pin its ordinary kind.
    // Checked first: `is_inert_pin` and `is_local` are exclusive (a pin has a commit,
    // a local form has none), but the pin case owns any `@sha`-carrying link.
    if w.is_inert_pin() {
        return SemanticTokenKind::WikilinkPinned {
            target: w.target.clone(),
            repo: w.repo.clone(),
            commit: w.commit.clone().unwrap_or_default(),
        };
    }
    // The LOCAL form (`[[^id]]` / `[[#head]]` / `[[^^id]]`) targets the source file
    // itself, per [[type reference::au-type-system]], so it is never dangling. Resolving it against the
    // repo index looks up an empty name and misreports it as broken.
    if w.is_local() && w.repo.is_none() {
        return SemanticTokenKind::WikilinkResolved {
            target: w.target.clone(),
            repo: w.repo.clone(),
            resolved: source.display().to_string(),
        };
    }
    let resolved =
        resolve_wikilink(kb, source, &w.target, w.repo.as_deref()).map(|p| p.display().to_string());
    let target = w.target.clone();
    let repo = w.repo.clone();
    match resolved {
        Some(resolved) => SemanticTokenKind::WikilinkResolved {
            target,
            repo,
            resolved,
        },
        None => SemanticTokenKind::WikilinkBroken { target, repo },
    }
}

/// A file-absolute `ByteRange` as a `SpanRange` with `line_col` attached.
fn located(
    span: au_diagnostics::ByteRange,
    lines: Option<&au_diagnostics::LineIndex>,
) -> wire::SpanRange {
    wire::SpanRange::new(span.start, span.end).located(lines)
}

/// Lift a body-relative `ByteRange` into a file-absolute `SpanRange` with
/// `line_col` attached. Body events span the markdown body, which starts at
/// `body_offset` into the physical file.
fn span_in_body(
    span: au_diagnostics::ByteRange,
    body_offset: usize,
    lines: Option<&au_diagnostics::LineIndex>,
) -> wire::SpanRange {
    wire::SpanRange::new(span.start + body_offset, span.end + body_offset).located(lines)
}

/// Resolve a navigation read's `target` to a file path, scoped to `origin`
/// when given. The `target` carries the wikilink fragment grammar, so an
/// embedded `::repo` qualifier is honored exactly as `references_out` resolves
/// a body link. With an `origin` the resolution is source-scoped, repo-local
/// for a bare target and cross-repo for a `::repo` one. Without an `origin` a
/// `::repo` target still resolves against the named repo's index, and a bare
/// target falls back to the scopeless union across every repo index, the
/// single-repo form. `None` when unresolved or ambiguous.
///
/// A commit-pinned target (`[[file::@sha]]` / `[[::@sha]]`) is an inert tombstone
/// into an immutable past: it resolves to NO live file, matching the backlink index
/// (`resolve_link`) and the outbound-pin contract (`references_out` carries
/// `resolved: null` for any commit-bearing edge). Live-resolving a pin by name would
/// misattribute on name reuse. See [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
fn resolve_navigation_target(
    kb: &KnowledgeBase,
    target: &str,
    origin: Option<&Path>,
) -> Option<PathBuf> {
    let parsed = parse_wikilink_inner(target).ok();
    // An inert pin (any `@sha`) never resolves to a live file. Checked first, before
    // the local and name branches, so a pin is a tombstone whatever else it carries.
    if parsed.as_ref().is_some_and(|w| w.is_inert_pin()) {
        return None;
    }
    // The LOCAL form (`[[^id]]` / `[[#head]]` / `[[^^id]]`, an empty target with a
    // locating fragment) resolves to the origin file itself, per [[type reference::au-type-system]].
    // Name lookup is skipped, so it never dangles — the same resolution the backlink
    // index (`resolve_link`) and the validator (`is_local`) perform. Without an
    // origin there is no file to be local to, so it resolves nothing.
    // The parse fallback trims, matching `parse_wikilink_inner`'s own trim, so a
    // whitespace-only target reads as empty (local) rather than a name lookup on blanks.
    let is_local = parsed.as_ref().map_or(target.trim().is_empty(), |w| {
        w.is_local() && w.repo.is_none()
    });
    if is_local {
        return origin.map(Path::to_path_buf);
    }
    let (name, repo) = match parsed {
        Some(w) => (w.target, w.repo),
        None => (target.to_string(), None),
    };
    match origin {
        Some(origin) => resolve_wikilink(kb, origin, &name, repo.as_deref()),
        None => match &repo {
            // A `::repo` qualifier names its own scope, so it resolves without
            // an origin: the named repo's index.
            Some(repo_q) => kb.index_for_repo(repo_q)?.resolve(&name).ok(),
            None => kb.resolve_any(&name).ok(),
        },
    }
}

/// Resolve a wikilink target; `None` when unresolved or ambiguous. Scoped to
/// `origin` (the file the link appears in) when given, so member-local and
/// `::repo`-qualified targets resolve the same as `references_out`; scopeless
/// otherwise. Carries the engine's classification beside the path, so a
/// consumer knows what it resolved to without re-deriving it from the
/// extension.
///
/// `source` carries an openable container path plus a container-relative span
/// when the resolved path is a type-def, the same `{ file, span }` the type
/// reads carry: a consumer opens `source.file` and reveals `source.span`. A
/// non-type-def `path` is itself a real, directly-openable file and `source` is
/// `null`.
fn resolve_target_view(
    kb: &KnowledgeBase,
    target: &str,
    origin: Option<&Path>,
) -> Option<serde_json::Value> {
    let path = resolve_navigation_target(kb, target, origin)?;
    let entry = kb.catalog.get(&path);
    let kind = entry
        .map(|e| match e.kind {
            au_parser::FileKind::TypeDef => "type-def",
            au_parser::FileKind::Instance => "instance",
            au_parser::FileKind::RepoRegistry => "repo-registry",
            au_parser::FileKind::Workspace => "workspace",
            au_parser::FileKind::RepoLock => "repo-lock",
            au_parser::FileKind::Unclassified => "unclassified",
        })
        .unwrap_or("unclassified");
    // The content hash doubles as the mutation channel's `expected_hash`
    // source; `null` for files the build never read.
    let hash = entry.and_then(|e| e.hash).map(crate::mutate::hash_hex);
    let source = match kb.file_parse(&path) {
        Some(FileParse::TypeDef {
            type_def: Some(def),
            ..
        }) => Some(wire::source_loc_at(kb, &path, def.source_span)),
        _ => None,
    };
    Some(serde_json::json!({
        "path": path.display().to_string(),
        "kind": kind,
        "hash": hash,
        "source": source,
    }))
}

/// Resolve `[[target^block-id]]` to its addressable entity per
/// [[type block-id::au-type-system]]: an inline record carrying `^: id` (checked first,
/// frontmatter precedes the body), else the first body occurrence — a
/// typed `[:field]` fence, an untyped fence id, or a bare `^id` marker.
/// The read is the one resolution surface, navigation included; only an
/// unresolvable target or an absent id returns `None`.
fn resolve_block_id_view(
    kb: &KnowledgeBase,
    target: &str,
    block_id: &str,
    origin: Option<&Path>,
) -> Option<ResolvedBlockDto> {
    let path = resolve_navigation_target(kb, target, origin)?;
    if let Some(FileParse::Instance {
        instance: Some(inst),
        ..
    }) = kb.file_parse(&path)
    {
        if let Some(record) =
            crate::resolution_build::record_targets_of_kb(kb, &path, inst).get(block_id)
        {
            return Some(ResolvedBlockDto {
                file_path: path.display().to_string(),
                // The qualified form, so a `::repo` record claim stays qualified
                // (the `qualified` field preserves it; `claims` is bare).
                type_claim: record.qualified.iter().map(|c| c.authored()).collect(),
                span: wire::SpanRange::new(record.span.start, record.span.end)
                    .located(kb.line_index(&path)),
                kind: "record",
            });
        }
    }
    let (body, body_offset) = kb.file_parse(&path)?.markdown_body()?;
    for ev in scan_body(body) {
        match ev {
            BodyEvent::FencedBlock {
                info,
                body: block_body,
                span,
                trailing_block_id: Some(id),
            } if id == block_id => {
                let typed = extract_field_marker(info).is_some();
                return Some(ResolvedBlockDto {
                    file_path: path.display().to_string(),
                    type_claim: if typed {
                        block_type_claim(block_body)
                    } else {
                        Vec::new()
                    },
                    span: wire::SpanRange::new(span.start + body_offset, span.end + body_offset)
                        .located(kb.line_index(&path)),
                    kind: if typed { "typed_block" } else { "marker" },
                });
            }
            BodyEvent::BlockIdMarker { id, span } if id == block_id => {
                return Some(ResolvedBlockDto {
                    file_path: path.display().to_string(),
                    type_claim: Vec::new(),
                    span: wire::SpanRange::new(span.start + body_offset, span.end + body_offset)
                        .located(kb.line_index(&path)),
                    kind: "marker",
                });
            }
            _ => {}
        }
    }
    None
}

/// Resolve a `#head` anchor to its heading line in the target file.
/// Matching is the engine's contract per [[type reference::au-type-system]] —
/// case-insensitive exact heading text, first match in document order —
/// so consumers never re-implement it client-side. `None` when the
/// target doesn't resolve, isn't a markdown instance, or no heading
/// matches.
fn resolve_anchor_view(
    kb: &KnowledgeBase,
    target: &str,
    anchor: &str,
    origin: Option<&Path>,
) -> Option<ResolvedAnchorDto> {
    let path = resolve_navigation_target(kb, target, origin)?;
    let (body, body_offset) = kb.file_parse(&path)?.markdown_body()?;
    let events = scan_body(body);
    let span = au_references::resolve_anchor(&events, anchor)?;
    Some(ResolvedAnchorDto {
        file_path: path.display().to_string(),
        span: wire::SpanRange::new(span.start + body_offset, span.end + body_offset)
            .located(kb.line_index(&path)),
    })
}

/// Every heading in a target file, in document order — the listing dual of
/// [`resolve_anchor_view`].
///
/// `None` only when the TARGET doesn't resolve, the catalog's unresolved-lookup
/// signal. A file that resolves but carries no markdown body (a pure-YAML
/// instance, an unread asset) answers an EMPTY array: it resolved, it simply
/// has no headings, and collapsing that into `None` would report "no such file"
/// for a file that exists.
fn anchors_view(kb: &KnowledgeBase, target: &str, origin: Option<&Path>) -> Option<Vec<AnchorDto>> {
    let path = resolve_navigation_target(kb, target, origin)?;
    let Some((body, body_offset)) = kb.file_parse(&path).and_then(|p| p.markdown_body()) else {
        return Some(Vec::new());
    };
    let line_index = kb.line_index(&path);
    Some(
        scan_body(body)
            .into_iter()
            .filter_map(|ev| match ev {
                BodyEvent::Heading { level, text, span } => Some(AnchorDto {
                    // Trimmed to match what `au_references::resolve_anchor`
                    // compares, so the served text round-trips through
                    // `[[file#<text>]]` by local guarantee rather than by
                    // agreement with the parser.
                    text: text.trim().to_string(),
                    level,
                    span: wire::SpanRange::new(span.start + body_offset, span.end + body_offset)
                        .located(line_index),
                }),
                _ => None,
            })
            .collect(),
    )
}

/// The resolvable file set: every catalogued file, path-sorted, paged.
///
/// The catalogue IS the resolvable set — the reference index is built from it,
/// so what is catalogued is what a wikilink can reach. That is why an ASSET
/// appears: the walker records it by path and never reads it, precisely so
/// `file*` resolves against it ([[type-def shape file::au-type-system]]). A consumer completing
/// `[[` must be able to offer a target the validator accepts.
///
/// A projection of the held catalogue, so no walk and no disk read.
fn files_view(
    kb: &KnowledgeBase,
    repo: Option<&str>,
    scope: wire::TypeScope,
    limit: usize,
    offset: usize,
) -> Vec<FileDto> {
    kb.catalog
        .iter()
        .filter(|(path, _)| {
            if repo.is_none() && !scope.own_only() {
                return true;
            }
            let Some(owner) = kb.repos.repo_of(path) else {
                // An unowned file cannot answer "is this mine", so it is out of
                // scope whenever either filter is engaged.
                return false;
            };
            if let Some(want) = repo {
                if owner.name.as_str() != want {
                    return false;
                }
            }
            scope.includes_repo(kb, owner)
        })
        .skip(offset)
        .take(limit)
        .map(|(path, _)| FileDto {
            path: path.display().to_string(),
            stem: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            repo: kb.repos.repo_of(path).map(|r| r.name.as_str().to_string()),
            kind: served_file_kind(kb.file_parse(path)),
        })
        .collect()
}

/// Every addressable id in a target file, in document order — the listing dual
/// of [`resolve_block_id_view`].
///
/// Both surfaces of [[type block-id::au-type-system]] in one stream: frontmatter inline records
/// carrying `^:`, and body occurrences (fence ids and bare markers). Sorted by
/// span, so frontmatter precedes the body the way the file does.
///
/// **Every occurrence is listed, duplicates included.** `resolve_block_id`
/// returns the FIRST and ignores the rest, which is right for resolution and
/// wrong for a listing: dropping the later ones would hide exactly what
/// `block-id-duplicate` reports. The listing is the file's surface, not the
/// resolver's verdict.
///
/// `None` only when the TARGET doesn't resolve, matching [`anchors_view`].
fn block_ids_view(
    kb: &KnowledgeBase,
    target: &str,
    origin: Option<&Path>,
) -> Option<Vec<BlockIdDto>> {
    let path = resolve_navigation_target(kb, target, origin)?;
    let line_index = kb.line_index(&path);
    let mut out: Vec<(usize, BlockIdDto)> = Vec::new();

    // Frontmatter records. Spans are already file-absolute.
    if let Some(FileParse::Instance {
        instance: Some(inst),
        ..
    }) = kb.file_parse(&path)
    {
        for (id, record) in
            crate::resolution_build::record_target_occurrences_of_kb(kb, &path, inst)
        {
            out.push((
                record.span.start,
                BlockIdDto {
                    id,
                    kind: "record",
                    // The qualified form, so a `::repo` record claim stays
                    // qualified, as `resolve_block_id` serves it.
                    type_claim: record.qualified.iter().map(|c| c.authored()).collect(),
                    span: wire::SpanRange::new(record.span.start, record.span.end)
                        .located(line_index),
                },
            ));
        }
    }

    // Body occurrences. Spans are body-relative, so shift by the offset.
    if let Some((body, body_offset)) = kb.file_parse(&path).and_then(|p| p.markdown_body()) {
        for ev in scan_body(body) {
            let (id, kind, type_claim, span) = match ev {
                BodyEvent::FencedBlock {
                    info,
                    body: block_body,
                    span,
                    trailing_block_id: Some(id),
                } => {
                    let typed = extract_field_marker(info).is_some();
                    (
                        id.to_string(),
                        if typed { "typed_block" } else { "marker" },
                        if typed {
                            block_type_claim(block_body)
                        } else {
                            Vec::new()
                        },
                        span,
                    )
                }
                BodyEvent::BlockIdMarker { id, span } => {
                    (id.to_string(), "marker", Vec::new(), span)
                }
                _ => continue,
            };
            let start = span.start + body_offset;
            out.push((
                start,
                BlockIdDto {
                    id,
                    kind,
                    type_claim,
                    span: wire::SpanRange::new(start, span.end + body_offset).located(line_index),
                },
            ));
        }
    }

    out.sort_by_key(|(start, _)| *start);
    Some(out.into_iter().map(|(_, dto)| dto).collect())
}

/// The `type:` claim of a marked fence's body, as a name list. Empty when
/// the body has no top-level `type:` key. A bare claim yields one name, a list
/// claim yields several.
fn block_type_claim(body: &str) -> Vec<String> {
    use au_parser::yaml::{parse, Scalar, YamlData};
    let Ok(docs) = parse(body) else {
        return Vec::new();
    };
    let Some(doc) = docs.first() else {
        return Vec::new();
    };
    let YamlData::Mapping(map) = &doc.data else {
        return Vec::new();
    };
    for (k, v) in map.iter() {
        if !matches!(&k.data, YamlData::Value(Scalar::String(s)) if s.as_ref() == "type") {
            continue;
        }
        return match &v.data {
            YamlData::Value(Scalar::String(name)) => vec![name.to_string()],
            YamlData::Sequence(items) => items
                .iter()
                .filter_map(|it| match &it.data {
                    YamlData::Value(Scalar::String(name)) => Some(name.to_string()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
    }
    Vec::new()
}

/// Direct children of a directory, files and subdirectories, hidden entries
/// (dot-prefixed) excluded. Derived from the held file catalog, so it reflects
/// the analyzed file set, the walker's ignore list already applied. Empty
/// directories do not appear, the catalog holds files, not bare directories.
fn children_view(kb: &KnowledgeBase, dir: &Path) -> Vec<DirEntryDto> {
    use std::collections::BTreeSet;
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut files: BTreeSet<PathBuf> = BTreeSet::new();
    // Path ordering keeps a directory's descendants contiguous, so seek to `dir`
    // and stop at the first key outside it, O(log n + descendants), not a scan
    // of the whole catalog. `take_while` on `starts_with` bounds the range.
    for (path, _) in kb
        .catalog
        .range(dir.to_path_buf()..)
        .take_while(|(p, _)| p.starts_with(dir))
    {
        if path.parent() == Some(dir) {
            files.insert(path.clone());
            continue;
        }
        // A descendant deeper than a direct child: its first component under
        // `dir` is a child directory. `strip_prefix` cannot fail here, the range
        // yields only descendants of `dir`.
        let Ok(rel) = path.strip_prefix(dir) else {
            continue;
        };
        let mut comps = rel.components();
        if let (Some(first), Some(_)) = (comps.next(), comps.next()) {
            dirs.insert(dir.join(first.as_os_str()));
        }
    }
    let mut out = Vec::new();
    for d in dirs {
        if let Some(name) = entry_name(&d) {
            out.push(DirEntryDto {
                path: d.display().to_string(),
                name,
                kind: "directory",
            });
        }
    }
    for f in files {
        if let Some(name) = entry_name(&f) {
            out.push(DirEntryDto {
                path: f.display().to_string(),
                name,
                kind: "file",
            });
        }
    }
    out
}

/// The top-level directories of EVERY mounted member, each tagged with its
/// owning repo: direct subdirectories holding at least one catalogued file,
/// hidden ones excluded, derived from the catalog.
///
/// Workspace-scoped, not entry-scoped. The former `top_level_graphs` walked
/// only the entry root, which answers approximately nothing in the expected
/// topology — a thin entry repo carrying `workspace.yaml` with the content in
/// `edit` members. It was also the sole entry-scoped field in an otherwise
/// workspace-scoped `overview`.
///
/// `repo` selects one member; absent spans every mounted one. `scope` filters
/// by CLASS: `own` keeps the user's editable repos, dropping dependencies whose
/// folders are noise for orientation and whose count grows with the transitive
/// dependency closure.
fn top_level_dirs_view(
    kb: &KnowledgeBase,
    repo: Option<&str>,
    scope: wire::TypeScope,
) -> Vec<TopLevelDirDto> {
    use std::collections::BTreeSet;
    let mut out: Vec<TopLevelDirDto> = Vec::new();
    for member in kb.repos.repos() {
        // The builtin `au-engine` repo is a compiled-in type source with no
        // real root on disk, so it has no directories to report.
        if member.builtin {
            continue;
        }
        if repo.is_some_and(|want| member.name.as_str() != want) {
            continue;
        }
        if !scope.includes_repo(kb, member) {
            continue;
        }
        let mut names: BTreeSet<String> = BTreeSet::new();
        for path in kb
            .catalog
            .range(member.root.clone()..)
            .take_while(|(p, _)| p.starts_with(&member.root))
            .map(|(p, _)| p)
        {
            let Ok(rel) = path.strip_prefix(&member.root) else {
                continue;
            };
            let mut comps = rel.components();
            if let (Some(first), Some(_)) = (comps.next(), comps.next()) {
                let name = first.as_os_str().to_string_lossy().to_string();
                if !name.starts_with('.') {
                    names.insert(name);
                }
            }
        }
        out.extend(names.into_iter().map(|name| {
            let path = member.root.join(&name);
            // A member rooted exactly here: the directory is another mounted
            // repo, reported for its container as well as for itself.
            let nested = kb
                .repos
                .repos()
                .iter()
                .find(|r| !r.builtin && r.root == path)
                .map(|r| r.name.as_str().to_string());
            TopLevelDirDto {
                repo: member.name.as_str().to_string(),
                path: path.display().to_string(),
                name,
                member: nested,
            }
        }));
    }
    out
}

/// The bounded top-N hubs the `overview` read carries, keeping the up-front
/// drop cheap. The standalone `hubs` read pages past it via `limit` / `offset`;
/// an absent `limit` there is this same bound, so `overview.hubs` mirrors an
/// argless call.
const HUB_LIMIT: usize = 20;

/// The default `neighborhood` node cap, applied when the caller names no
/// `max_nodes`. A safety ceiling on an unbounded walk, generous enough that the
/// common one- and two-hop calls never hit it, loud (`truncated` + `dropped`)
/// when they do.
const NEIGHBORHOOD_MAX_NODES: usize = 2000;

/// The `overview` read result: the up-front orientation map. Each field reuses
/// the shape of its own read, so a consumer reuses the DTOs it already holds.
#[derive(Debug, Serialize)]
struct OverviewView {
    /// The RESOLVED `repo` this map was computed at, echoed back. Null spans
    /// every mounted member.
    repo: Option<String>,
    /// The RESOLVED `scope` this map was computed at, echoed back — `own`
    /// unless the caller said otherwise.
    ///
    /// Echoed so a consumer drilling from a field into its own read passes
    /// these verbatim rather than re-deriving a default it cannot see. Without
    /// it, `overview({}).type_counts` and `type_counts({})` disagree silently:
    /// the summary card and the panel it opens would show different numbers,
    /// each plausible, neither an error. The catalog already echoes an argument
    /// this way (`subtypes.base`), and the envelope rule names an echoed
    /// argument as legitimate envelope metadata.
    scope: &'static str,
    // The members array directly (not the `members` read's `{ members: [...] }`
    // wrapper), so a consumer reads `overview.members`, not `.members.members`.
    members: Vec<wire::MemberView>,
    top_level_dirs: Vec<TopLevelDirDto>,
    type_counts: wire::TypeCountsView,
    diagnostic_counts: DiagnosticCountsView,
    hubs: Vec<Hub>,
    /// The scalar whole-graph summary, mirroring an argless `graph_shape` call
    /// at these same `repo` / `scope` args. The unbounded orphan path list is a
    /// summary card, so `orphan_paths` is off here — a consumer wanting it drills
    /// the `graph_shape` read.
    graph_shape: GraphShapeView,
}

/// Whether a file is in scope for a whole-graph fold, the shared `repo` /
/// `scope` predicate `hubs`, `graph_shape`, and `link_graph` apply so a second
/// spelling of "the user's own repos" cannot drift between them.
///
/// `repo` absent with `scope: all` is the fast path, everything in scope with no
/// owner resolution. Otherwise resolve the owning repo and gate on the named
/// `repo` and the `own`-vs-`all` class.
fn node_in_scope(
    kb: &KnowledgeBase,
    repo: Option<&str>,
    scope: wire::TypeScope,
    path: &Path,
) -> bool {
    if repo.is_none() && !scope.own_only() {
        return true;
    }
    let Some(owner) = kb.repos.repo_of(path) else {
        return false;
    };
    if let Some(want) = repo {
        if owner.name.as_str() != want {
            return false;
        }
    }
    scope.includes_repo(kb, owner)
}

/// Whether a catalog file is a knowledge-graph node. Engine-schema files
/// (`repo.yaml`, the workspace manifest, the `repo.lock`) are substrate CONFIG,
/// not knowledge, so they never count as a node, an orphan, or a component: a
/// consumer's orphan list must not surface `.arsumbris/repo.yaml`, and a graph
/// view must not draw it. The content kinds are type-defs, instances, and
/// everything else the walk catalogues (notes and assets, both `Unclassified`).
fn is_graph_node_kind(kind: au_parser::FileKind) -> bool {
    use au_parser::FileKind;
    matches!(
        kind,
        FileKind::TypeDef | FileKind::Instance | FileKind::Unclassified
    )
}

/// A disjoint-set forest with path halving and union by rank, for the
/// connected-component fold. Nodes are catalog indices.
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        match self.rank[ra].cmp(&self.rank[rb]) {
            std::cmp::Ordering::Less => self.parent[ra] = rb,
            std::cmp::Ordering::Greater => self.parent[rb] = ra,
            std::cmp::Ordering::Equal => {
                self.parent[rb] = ra;
                self.rank[ra] += 1;
            }
        }
    }
}

/// The scalar whole-graph summary. Fold the in-scope catalog (the node universe)
/// and the in-scope backlink index (edges induced on in-scope endpoints) into
/// component count, orphan counts, degree histograms, and density. One
/// union-find pass yields components and the largest component; the same edge
/// walk yields the degrees, and the node walk the orphans and histograms.
///
/// Multiplicity matches `hubs`: a target's in-degree is `edges.len()`, so
/// `refs_total` on a `hubs` entry equals this node's in-degree. Component
/// connectivity ignores multiplicity.
fn graph_shape_view(
    kb: &KnowledgeBase,
    repo: Option<&str>,
    scope: wire::TypeScope,
    orphan_paths: bool,
) -> GraphShapeView {
    // The in-scope node universe, indexed for the union-find and the edge walk.
    let nodes: Vec<&Path> = kb
        .catalog
        .iter()
        .filter(|(_, e)| is_graph_node_kind(e.kind))
        .map(|(p, _)| p.as_path())
        .filter(|p| node_in_scope(kb, repo, scope, p))
        .collect();
    let index: std::collections::HashMap<&Path, usize> =
        nodes.iter().enumerate().map(|(i, p)| (*p, i)).collect();
    let n = nodes.len();

    let mut uf = UnionFind::new(n);
    let mut in_degree = vec![0usize; n];
    let mut out_degree = vec![0usize; n];
    let mut edges_structural = 0usize;
    let mut edges_navigational = 0usize;

    for (target, edges) in kb.backlinks.iter() {
        let Some(&ti) = index.get(target.as_path()) else {
            continue;
        };
        for edge in edges {
            // An edge counts only when BOTH endpoints are in scope; an
            // out-of-scope source is not a node here, so the edge is dropped.
            let Some(&si) = index.get(edge.source.as_path()) else {
                continue;
            };
            uf.union(si, ti);
            in_degree[ti] += 1;
            out_degree[si] += 1;
            if edge.slot.is_some() {
                edges_structural += 1;
            } else {
                edges_navigational += 1;
            }
        }
    }
    let edge_count = edges_structural + edges_navigational;

    // Component sizes from the union-find roots.
    let mut comp_size: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        *comp_size.entry(root).or_default() += 1;
    }
    let components = comp_size.len();
    let largest_component = comp_size.values().copied().max().unwrap_or(0);

    // Orphans and degree histograms in one node walk.
    let mut no_inbound = 0usize;
    let mut isolated = 0usize;
    let mut in_hist: BTreeMap<usize, usize> = BTreeMap::new();
    let mut out_hist: BTreeMap<usize, usize> = BTreeMap::new();
    let mut paths: Vec<OrphanPath> = Vec::new();
    for i in 0..n {
        *in_hist.entry(in_degree[i]).or_default() += 1;
        *out_hist.entry(out_degree[i]).or_default() += 1;
        if in_degree[i] == 0 {
            no_inbound += 1;
            let kind = if out_degree[i] == 0 {
                isolated += 1;
                "isolated"
            } else {
                "no_inbound"
            };
            if orphan_paths {
                paths.push(OrphanPath {
                    path: nodes[i].display().to_string(),
                    kind,
                });
            }
        }
    }
    if orphan_paths {
        paths.sort_by(|a, b| a.path.cmp(&b.path));
    }

    let density = if n == 0 {
        0.0
    } else {
        edge_count as f64 / n as f64
    };

    let to_buckets = |hist: BTreeMap<usize, usize>| {
        hist.into_iter()
            .map(|(degree, count)| DegreeBucket { degree, count })
            .collect()
    };

    GraphShapeView {
        repo: repo.map(|s| s.to_string()),
        scope: if scope.own_only() { "own" } else { "all" },
        node_count: n,
        edge_count,
        edges_structural,
        edges_navigational,
        components,
        largest_component,
        orphans: OrphansView {
            no_inbound,
            isolated,
            paths: orphan_paths.then_some(paths),
        },
        degree: DegreeView {
            inbound: to_buckets(in_hist),
            out: to_buckets(out_hist),
        },
        density,
    }
}

/// The full whole-graph node+edge payload. Nodes are the in-scope content
/// catalog (isolated files included); edges are the resolved backlink index
/// induced on in-scope endpoints. Node ref counts are over the in-scope inbound
/// edges, so a node's `refs_total` equals its inbound-edge count in the same
/// payload. Nodes and edges are sorted for a deterministic result.
fn link_graph_view(
    kb: &KnowledgeBase,
    repo: Option<&str>,
    scope: wire::TypeScope,
) -> LinkGraphView {
    use crate::backlinks::RefSurface;

    // The in-scope content node set, the shared universe with `graph_shape`.
    let node_set: std::collections::HashSet<&Path> = kb
        .catalog
        .iter()
        .filter(|(_, e)| is_graph_node_kind(e.kind))
        .map(|(p, _)| p.as_path())
        .filter(|p| node_in_scope(kb, repo, scope, p))
        .collect();

    let mut refs_structural: std::collections::HashMap<&Path, usize> =
        std::collections::HashMap::new();
    let mut refs_total: std::collections::HashMap<&Path, usize> = std::collections::HashMap::new();
    let mut edges: Vec<GraphEdgeView> = Vec::new();

    for (target, bls) in kb.backlinks.iter() {
        let tp = target.as_path();
        if !node_set.contains(&tp) {
            continue;
        }
        for b in bls {
            // Both endpoints in scope, else the edge is out of the induced graph.
            let sp = b.source.as_path();
            if !node_set.contains(&sp) {
                continue;
            }
            *refs_total.entry(tp).or_default() += 1;
            if b.slot.is_some() {
                *refs_structural.entry(tp).or_default() += 1;
            }
            edges.push(GraphEdgeView {
                from: sp.display().to_string(),
                to: tp.display().to_string(),
                kind: crate::backlinks::coarse_edge_kind(b.surface, b.slot.as_deref()),
                surface: match b.surface {
                    RefSurface::Frontmatter => "frontmatter",
                    RefSurface::Body => "body",
                    RefSurface::Docstring => "docstring",
                },
            });
        }
    }

    let mut nodes: Vec<GraphNodeView> = node_set
        .iter()
        .map(|&p| GraphNodeView {
            path: p.display().to_string(),
            repo: kb.repos.repo_of(p).map(|r| r.name.as_str().to_string()),
            kind: served_file_kind(kb.file_parse(p)),
            refs_structural: refs_structural.get(&p).copied().unwrap_or(0),
            refs_total: refs_total.get(&p).copied().unwrap_or(0),
        })
        .collect();

    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    edges.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then_with(|| a.to.cmp(&b.to))
            .then_with(|| a.kind.cmp(b.kind))
            .then_with(|| a.surface.cmp(b.surface))
    });

    LinkGraphView {
        repo: repo.map(|s| s.to_string()),
        scope: if scope.own_only() { "own" } else { "all" },
        nodes,
        edges,
    }
}

/// The edge classes a `type_graph` read may request. An unknown value fails
/// deserialization (a malformed-read error frame), never a silent default. The
/// authored forms are `subtype` / `field-type` / `instance-of` / `meta`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum EdgeClass {
    Subtype,
    FieldType,
    InstanceOf,
    Meta,
}

/// The resolved edge-class selection for a `type_graph` fold. Absent `edges`
/// defaults to the type-to-type backbone (`subtype` + `field-type`);
/// `instance-of` and `meta` are opt-in, since `instance-of` pulls every claiming
/// instance in as a node.
#[derive(Debug, Clone, Copy)]
struct EdgeClasses {
    subtype: bool,
    field_type: bool,
    instance_of: bool,
    meta: bool,
}

impl EdgeClasses {
    fn from_arg(edges: Option<Vec<EdgeClass>>) -> Self {
        match edges {
            None => Self {
                subtype: true,
                field_type: true,
                instance_of: false,
                meta: false,
            },
            Some(list) => Self {
                subtype: list.contains(&EdgeClass::Subtype),
                field_type: list.contains(&EdgeClass::FieldType),
                instance_of: list.contains(&EdgeClass::InstanceOf),
                meta: list.contains(&EdgeClass::Meta),
            },
        }
    }
}

/// One node of the `type_graph` payload: a type-def, or an instance when
/// `instance-of` is requested. The node identity is `path`, so the payload
/// merges with `link_graph` by path. Node sizing is the consumer's, derived from
/// the edges it receives (subtype / instance-of in-degree); the reference-graph
/// ref counts stay on `link_graph`, the reference graph's own concern.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct TypeGraphNodeView {
    path: String,
    repo: Option<String>,
    kind: &'static str,
}

/// One edge of the `type_graph` payload. `relation` is the edge class
/// (`subtype` / `field-type` / `instance-of` / `meta`); `count` is the deduped
/// multiplicity, the number of field references to the target folded into one
/// `field-type` edge (an edge weight; a field whose shape names the target twice
/// counts twice). The other relations are single by construction, so `count` is
/// 1.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct TypeGraphEdgeView {
    from: String,
    to: String,
    relation: &'static str,
    count: usize,
}

/// The `type_graph` read result: the schema graph as a node+edge payload, the
/// type-side sibling of `link_graph`. `nodes` are the in-scope type-defs (plus
/// claiming instances when `instance-of` is on); `edges` are the resolved
/// type-graph edges induced on in-scope endpoints. Sorted for a deterministic
/// payload.
#[derive(Debug, Serialize)]
struct TypeGraphView {
    repo: Option<String>,
    scope: &'static str,
    nodes: Vec<TypeGraphNodeView>,
    edges: Vec<TypeGraphEdgeView>,
}

/// Fold the held type graph into a node+edge payload, the type-side sibling of
/// [`link_graph_view`]. Nodes are in-scope type-def files (plus claiming
/// instances when `instance-of` is requested); edges are the resolved
/// subtype / field-type / meta / instance-of relations, induced on in-scope
/// endpoints (a cross-repo edge whose target is out of scope drops, the same
/// rule `link_graph` uses). `field-type` edges dedup across fields into one edge
/// per pair carrying the field count.
fn type_graph_view(
    kb: &KnowledgeBase,
    repo: Option<&str>,
    scope: wire::TypeScope,
    classes: EdgeClasses,
) -> TypeGraphView {
    // The in-scope type-def node universe, the always-present nodes.
    let typedef_paths: std::collections::HashSet<&Path> = kb
        .catalog
        .iter()
        .filter(|(_, e)| matches!(e.kind, au_parser::FileKind::TypeDef))
        .map(|(p, _)| p.as_path())
        .filter(|p| node_in_scope(kb, repo, scope, p))
        .collect();

    // Resolve a `(name, ::repo)` type reference to a type-def source path: a
    // bare name in the referrer's OWN repo graph, a `::repo` name in the named
    // peer's graph. File-level resolution is unambiguous (a repo owns one type
    // of a name), so the per-repo `TypeGraph` suffices; identity/hash concerns
    // do not bear on which FILE is repo X's type Y.
    let resolve =
        |from_repo: Option<&str>, name: &str, repo_qual: Option<&str>| -> Option<PathBuf> {
            let graph = match repo_qual {
                Some(dep) => kb.graph_for_repo(dep)?,
                None => kb.graph_for_repo(from_repo?)?,
            };
            graph
                .get(&au_core::TypeName(name.to_string()))
                .map(|d| d.source_path.clone())
        };

    // Edges keyed by (from, to, relation) accumulate a count; a field-type pair
    // reached through several fields folds into one edge weighted by the count.
    let mut edge_counts: std::collections::HashMap<(PathBuf, PathBuf, &'static str), usize> =
        std::collections::HashMap::new();
    let mut instance_nodes: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    let mut add_edge = |from: &Path, to: PathBuf, relation: &'static str| {
        *edge_counts
            .entry((from.to_path_buf(), to, relation))
            .or_default() += 1;
    };

    // Type-to-type edges: walk each in-scope type-def once.
    for &path in &typedef_paths {
        let from_repo = kb.repos.repo_of(path).map(|r| r.name.as_str());
        let Some(FileParse::TypeDef {
            type_def: Some(def),
            ..
        }) = kb.file_parse(path)
        else {
            continue;
        };

        if classes.subtype {
            for parent in &def.parents {
                if let Some(tp) = resolve(from_repo, parent.name.as_str(), parent.repo.as_deref()) {
                    if typedef_paths.contains(tp.as_path()) {
                        add_edge(path, tp, "subtype");
                    }
                }
            }
        }
        if classes.field_type {
            for (base, repo_qual) in au_core::field_type_refs(def) {
                if let Some(tp) = resolve(from_repo, &base, repo_qual.as_deref()) {
                    if typedef_paths.contains(tp.as_path()) {
                        add_edge(path, tp, "field-type");
                    }
                }
            }
        }
        if classes.meta {
            if let Some(blocks) = &def.meta_blocks {
                for mb in blocks {
                    if let Some(tp) = resolve(from_repo, mb.type_name.as_str(), mb.repo.as_deref())
                    {
                        if typedef_paths.contains(tp.as_path()) {
                            add_edge(path, tp, "meta");
                        }
                    }
                }
            }
        }
    }

    // Instance-of edges: each in-scope instance's claims to an in-scope type-def.
    // A claiming instance becomes a node; a claim onto an out-of-scope type drops.
    if classes.instance_of {
        for (path, entry) in kb.catalog.iter() {
            if !matches!(entry.kind, au_parser::FileKind::Instance) {
                continue;
            }
            if !node_in_scope(kb, repo, scope, path) {
                continue;
            }
            let from_repo = kb.repos.repo_of(path).map(|r| r.name.as_str());
            let Some(FileParse::Instance {
                instance: Some(inst),
                ..
            }) = kb.file_parse(path)
            else {
                continue;
            };
            for claim in inst.type_claim.iter() {
                if let Some(tp) = resolve(from_repo, claim.name.as_str(), claim.repo.as_deref()) {
                    if typedef_paths.contains(tp.as_path()) {
                        add_edge(path, tp, "instance-of");
                        instance_nodes.insert(path.to_path_buf());
                    }
                }
            }
        }
    }

    // Nodes: the type-defs, plus any instance that gained an instance-of edge.
    let mut nodes: Vec<TypeGraphNodeView> = typedef_paths
        .iter()
        .map(|p| p.to_path_buf())
        .chain(instance_nodes.iter().cloned())
        .map(|p| TypeGraphNodeView {
            path: p.display().to_string(),
            repo: kb.repos.repo_of(&p).map(|r| r.name.as_str().to_string()),
            kind: served_file_kind(kb.file_parse(&p)),
        })
        .collect();
    nodes.sort_by(|a, b| a.path.cmp(&b.path));

    let mut edges: Vec<TypeGraphEdgeView> = edge_counts
        .into_iter()
        .map(|((from, to, relation), count)| TypeGraphEdgeView {
            from: from.display().to_string(),
            to: to.display().to_string(),
            relation,
            count,
        })
        .collect();
    edges.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then_with(|| a.to.cmp(&b.to))
            .then_with(|| a.relation.cmp(b.relation))
    });

    TypeGraphView {
        repo: repo.map(|s| s.to_string()),
        scope: if scope.own_only() { "own" } else { "all" },
        nodes,
        edges,
    }
}

/// The most-referenced files over the typed reference graph, folded from the
/// backlink index, the top `limit`. Per target, inbound edges split by
/// `slot.is_some()` into structural (fills a typed slot) and navigational
/// (prose). Ranked structural desc, then total desc, then path for stability.
/// Each hub carries its kind and owning repo. A file with no inbound edges is
/// absent, the backlink index only holds referenced files.
fn hub_ranking(
    kb: &KnowledgeBase,
    repo: Option<&str>,
    scope: wire::TypeScope,
    limit: usize,
    offset: usize,
) -> Vec<Hub> {
    let mut hubs: Vec<Hub> = kb
        .backlinks
        .iter()
        // Scope BEFORE ranking and truncation, which is the whole point of the
        // filter: `limit` used to cut the workspace-wide ranking first, so a
        // large dependency could fill every slot and crowd the user's own
        // content out of the top-N entirely. Filtering after would return fewer
        // than `limit` own hubs while own hubs existed.
        .filter(|(target, _)| node_in_scope(kb, repo, scope, target))
        .map(|(target, edges)| {
            let refs_structural = edges.iter().filter(|b| b.slot.is_some()).count();
            let refs_total = edges.len();
            Hub {
                path: target.display().to_string(),
                repo: kb
                    .repos
                    .repo_of(target)
                    .map(|r| r.name.as_str().to_string()),
                kind: served_file_kind(kb.file_parse(target)),
                refs_structural,
                refs_navigational: refs_total - refs_structural,
                refs_total,
            }
        })
        .collect();
    hubs.sort_by(|a, b| {
        b.refs_structural
            .cmp(&a.refs_structural)
            .then_with(|| b.refs_total.cmp(&a.refs_total))
            .then_with(|| a.path.cmp(&b.path))
    });
    hubs.into_iter().skip(offset).take(limit).collect()
}

#[cfg(test)]
mod hub_ranking_tests {
    use super::*;
    use au_parser::MemoryFileSystem;

    #[test]
    fn ranks_structural_over_navigational_and_excludes_leaves() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  title: String\n  rel: note*\n".to_vec(),
        );
        // A structural hub: an instance many others point their `rel` slot at.
        fs.insert("/v/hub.md", b"---\ntype: note\ntitle: hub\n---\n".to_vec());
        // A navigational hub: a plain note many others prose-link.
        fs.insert("/v/moc.md", b"# moc\n".to_vec());
        // A leaf: an instance nobody references.
        fs.insert(
            "/v/leaf.md",
            b"---\ntype: note\ntitle: leaf\n---\n".to_vec(),
        );
        for i in 0..5 {
            // A frontmatter wikilink is a quoted string; unquoted `[[hub]]` is a
            // YAML nested sequence, not a reference.
            let body =
                format!("---\ntype: note\ntitle: r{i}\nrel: \"[[hub]]\"\n---\n\nsee [[moc]].\n");
            fs.insert(format!("/v/ref-{i}.md"), body.into_bytes());
        }
        let kb = crate::build::build(Path::new("/v"), &fs).unwrap();

        let hubs = hub_ranking(&kb, None, wire::TypeScope::all(), 10, 0);

        // Structural hub ranks first, five `rel` edges, no prose edges, an instance.
        let hub = &hubs[0];
        assert!(
            hub.path.ends_with("/hub.md"),
            "structural hub first: {}",
            hub.path
        );
        assert_eq!(hub.kind, "instance");
        assert_eq!(hub.refs_structural, 5);
        assert_eq!(hub.refs_navigational, 0);
        assert_eq!(hub.repo.as_deref(), Some("v"));

        // The MOC ranks below it, five prose edges, no structural, a note.
        let moc_pos = hubs
            .iter()
            .position(|h| h.path.ends_with("/moc.md"))
            .unwrap();
        assert!(
            moc_pos > 0,
            "navigational hub ranks below the structural one"
        );
        let moc = &hubs[moc_pos];
        assert_eq!(moc.kind, "note");
        assert_eq!(moc.refs_structural, 0);
        assert_eq!(moc.refs_navigational, 5);

        // A file with no inbound edges never appears.
        assert!(!hubs.iter().any(|h| h.path.ends_with("/leaf.md")));
    }

    #[test]
    fn truncates_to_the_limit() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        // Ten notes each prose-linking the next, so ten targets have one edge.
        for i in 0..10 {
            let next = (i + 1) % 10;
            fs.insert(
                format!("/v/n-{i}.md"),
                format!("see [[n-{next}]].\n").into_bytes(),
            );
        }
        let kb = crate::build::build(Path::new("/v"), &fs).unwrap();
        assert_eq!(
            hub_ranking(&kb, None, wire::TypeScope::all(), 3, 0).len(),
            3,
            "top-N is bounded by the limit"
        );
    }
}

#[cfg(test)]
mod graph_shape_tests {
    use super::*;
    use au_parser::MemoryFileSystem;

    /// A vault of plain notes with a known shape:
    /// - `a → b`, `a → c` (one island of three).
    /// - `d → e` (a second island of two).
    /// - `lonely` (nothing, either way — a singleton island).
    /// - `out → a` (points in, unreferenced — a `no_inbound` orphan that is NOT
    ///   `isolated`, and it joins the first island).
    fn known_shape_fs() -> MemoryFileSystem {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert("/v/a.md", b"see [[b]] and [[c]].\n".to_vec());
        fs.insert("/v/b.md", b"# b\n".to_vec());
        fs.insert("/v/c.md", b"# c\n".to_vec());
        fs.insert("/v/d.md", b"see [[e]].\n".to_vec());
        fs.insert("/v/e.md", b"# e\n".to_vec());
        fs.insert("/v/lonely.md", b"# lonely\n".to_vec());
        fs.insert("/v/out.md", b"see [[a]].\n".to_vec());
        fs
    }

    #[test]
    fn folds_a_known_shape() {
        let kb = crate::build::build(Path::new("/v"), &known_shape_fs()).unwrap();
        let s = graph_shape_view(&kb, None, wire::TypeScope::all(), true);

        // The 7 content notes; `.arsumbris/repo.yaml` is engine-schema, not a node.
        assert_eq!(s.node_count, 7);
        // Three islands: {a,b,c,out}, {d,e}, {lonely}.
        assert_eq!(s.components, 3);
        assert_eq!(s.largest_component, 4);
        // Four navigational edges: a→b, a→c, d→e, out→a. No typed slots here.
        assert_eq!(s.edge_count, 4);
        assert_eq!(s.edges_structural, 0);
        assert_eq!(s.edges_navigational, 4);
        // `no_inbound`: d, lonely, out. `isolated` (also no outbound): lonely only.
        assert_eq!(s.orphans.no_inbound, 3);
        assert_eq!(s.orphans.isolated, 1);
        assert_eq!((s.density * 1000.0).round(), 571.0, "4 edges / 7 nodes");

        // The path list tags each orphan by its kind, sorted by path.
        let paths = s.orphans.paths.expect("orphan_paths requested");
        let by_path: std::collections::HashMap<&str, &str> =
            paths.iter().map(|o| (o.path.as_str(), o.kind)).collect();
        assert_eq!(by_path.get("/v/lonely.md"), Some(&"isolated"));
        assert_eq!(by_path.get("/v/d.md"), Some(&"no_inbound"));
        assert_eq!(by_path.get("/v/out.md"), Some(&"no_inbound"));

        // In-degree histogram: 3 nodes with 0 inbound, 4 with 1.
        assert_eq!(
            s.degree.inbound,
            vec![
                DegreeBucket {
                    degree: 0,
                    count: 3
                },
                DegreeBucket {
                    degree: 1,
                    count: 4
                },
            ]
        );
    }

    #[test]
    fn orphan_paths_flag_gates_the_list() {
        let kb = crate::build::build(Path::new("/v"), &known_shape_fs()).unwrap();
        let s = graph_shape_view(&kb, None, wire::TypeScope::all(), false);
        // Counts are always present; the unbounded path list is opt-in.
        assert_eq!(s.orphans.no_inbound, 3);
        assert_eq!(s.orphans.isolated, 1);
        assert!(s.orphans.paths.is_none());
    }

    #[test]
    fn splits_structural_from_navigational_edges() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  title: String\n  rel: note*\n".to_vec(),
        );
        fs.insert("/v/hub.md", b"---\ntype: note\ntitle: hub\n---\n".to_vec());
        // One structural edge (the `rel` slot) and one navigational edge (prose),
        // both pointing at hub.
        fs.insert(
            "/v/r.md",
            b"---\ntype: note\ntitle: r\nrel: \"[[hub]]\"\n---\n\nsee [[hub]].\n".to_vec(),
        );
        let kb = crate::build::build(Path::new("/v"), &fs).unwrap();
        let s = graph_shape_view(&kb, None, wire::TypeScope::all(), false);
        assert_eq!(s.edges_structural, 1, "the rel slot edge");
        assert_eq!(s.edges_navigational, 1, "the prose edge");
        assert_eq!(s.edge_count, 2);
    }

    #[test]
    fn link_graph_carries_every_content_node_and_resolved_edge() {
        let kb = crate::build::build(Path::new("/v"), &known_shape_fs()).unwrap();
        let g = link_graph_view(&kb, None, wire::TypeScope::all());

        // The 7 content notes, isolated ones included; repo.yaml is not a node.
        assert_eq!(g.nodes.len(), 7);
        let node = |p: &str| g.nodes.iter().find(|n| n.path == p).expect("node present");
        // The isolated node is present with zero refs.
        let lonely = node("/v/lonely.md");
        assert_eq!((lonely.refs_structural, lonely.refs_total), (0, 0));
        assert_eq!(lonely.kind, "note");
        // `a` is pointed at once (by out), navigationally.
        assert_eq!(node("/v/a.md").refs_total, 1);
        assert_eq!(node("/v/a.md").refs_structural, 0);

        // Four resolved edges, all body/navigational, both endpoints content.
        assert_eq!(g.edges.len(), 4);
        assert!(g
            .edges
            .iter()
            .all(|e| e.surface == "body" && e.kind == "navigational"));
        assert!(g
            .edges
            .iter()
            .any(|e| e.from == "/v/out.md" && e.to == "/v/a.md"));
        // Deterministic: sorted by (from, to, ...).
        let mut sorted = g.edges.clone();
        sorted.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));
        assert_eq!(g.edges, sorted);
    }

    #[test]
    fn link_graph_edge_kinds_match_references_in() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  title: String\n  rel: note*\n".to_vec(),
        );
        fs.insert("/v/hub.md", b"---\ntype: note\ntitle: hub\n---\n".to_vec());
        fs.insert(
            "/v/r.md",
            b"---\ntype: note\ntitle: r\nrel: \"[[hub]]\"\n---\n\nsee [[hub]].\n".to_vec(),
        );
        let kb = crate::build::build(Path::new("/v"), &fs).unwrap();
        let g = link_graph_view(&kb, None, wire::TypeScope::all());

        // The frontmatter `rel` edge is `field` / `frontmatter`; the prose edge
        // is `navigational` / `body`. hub carries both.
        let hub = g.nodes.iter().find(|n| n.path == "/v/hub.md").unwrap();
        assert_eq!((hub.refs_structural, hub.refs_total), (1, 2));
        assert!(g
            .edges
            .iter()
            .any(|e| e.to == "/v/hub.md" && e.kind == "field" && e.surface == "frontmatter"));
        assert!(g
            .edges
            .iter()
            .any(|e| e.to == "/v/hub.md" && e.kind == "navigational" && e.surface == "body"));
    }

    /// Two catalogs differing by one added prose link `a → b`.
    fn before_after() -> (LinkGraphView, LinkGraphView) {
        let mut before = MemoryFileSystem::new();
        before.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        before.insert("/v/a.md", b"# a\n".to_vec());
        before.insert("/v/b.md", b"# b\n".to_vec());
        let kb1 = crate::build::build(Path::new("/v"), &before).unwrap();

        let mut after = MemoryFileSystem::new();
        after.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        after.insert("/v/a.md", b"see [[b]].\n".to_vec());
        after.insert("/v/b.md", b"# b\n".to_vec());
        let kb2 = crate::build::build(Path::new("/v"), &after).unwrap();

        (
            link_graph_view(&kb1, None, wire::TypeScope::all()),
            link_graph_view(&kb2, None, wire::TypeScope::all()),
        )
    }

    #[test]
    fn delta_reports_the_added_edge_and_the_re_referenced_node() {
        let (before, after) = before_after();
        let d = link_graph_delta(&before, &after);

        // The added edge.
        assert_eq!(d.edges_added.len(), 1);
        assert_eq!(
            (d.edges_added[0].from.as_str(), d.edges_added[0].to.as_str()),
            ("/v/a.md", "/v/b.md")
        );
        assert!(d.edges_removed.is_empty());

        // b's record changed (0 → 1 inbound), so it re-emits; a is unchanged.
        let added_paths: Vec<&str> = d.nodes_added.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(added_paths, vec!["/v/b.md"]);
        assert_eq!(d.nodes_added[0].refs_total, 1);
        assert!(d.nodes_removed.is_empty());
    }

    #[test]
    fn delta_is_empty_when_the_graph_is_unchanged() {
        let (_before, after) = before_after();
        assert!(link_graph_delta(&after, &after).is_empty());
    }

    #[test]
    fn edge_multiset_diff_is_multiplicity_correct() {
        let e = |from: &str| GraphEdgeView {
            from: from.to_string(),
            to: "/v/t.md".to_string(),
            kind: "navigational",
            surface: "body",
        };
        // Two identical edges dropped to one yields exactly one removal.
        let (added, removed) = edge_multiset_diff(&[e("/v/s.md"), e("/v/s.md")], &[e("/v/s.md")]);
        assert!(added.is_empty());
        assert_eq!(removed.len(), 1);
        // The mirror: one grown to two yields one addition.
        let (added, removed) = edge_multiset_diff(&[e("/v/s.md")], &[e("/v/s.md"), e("/v/s.md")]);
        assert_eq!(added.len(), 1);
        assert!(removed.is_empty());
    }
}

/// The basename of a path as a string, `None` for the hidden (dot-prefixed)
/// entries the repo-files contract excludes.
fn entry_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_string();
    if name.starts_with('.') {
        None
    } else {
        Some(name)
    }
}

/// Read one length-prefixed frame off an async reader: a 4-byte big-endian
/// length, then that many bytes. `None` at a clean EOF before a frame. Generic
/// over the reader so it serves both a whole stream and a split read half.
async fn read_frame_async<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("frame length {len} exceeds maximum {MAX_FRAME}"),
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// Write one length-prefixed frame to an async writer.
async fn write_frame_async<W: tokio::io::AsyncWrite + Unpin>(
    stream: &mut W,
    bytes: &[u8],
) -> std::io::Result<()> {
    let len = bytes.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(bytes).await?;
    stream.flush().await
}

/// Read one length-prefixed frame off the sync client socket. `None` at a clean
/// EOF before a frame.
fn read_frame(stream: &mut UnixStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("frame length {len} exceeds maximum {MAX_FRAME}"),
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Write one length-prefixed frame to the sync client socket.
fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> std::io::Result<()> {
    let len = bytes.len() as u32;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(bytes)?;
    stream.flush()
}

/// How long a request-reply [`Client::query`] waits for its reply before giving
/// up. A wedged or unreachable daemon must surface as a timeout error, never an
/// infinite freeze. Generous, so a legitimately slow reply (a large workspace's
/// resolve) still lands; it is a safety net, not a latency budget.
/// Subscriptions opt out, they idle between pushed events.
const CLIENT_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// A short-lived client for the framed wire. For driving and testing the
/// endpoint from another process or thread.
pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// Connect to a served socket.
    pub fn connect(socket: impl AsRef<Path>) -> std::io::Result<Client> {
        Ok(Client {
            stream: UnixStream::connect(socket)?,
        })
    }

    /// Send a request (a JSON value naming a `read`) and read the response.
    ///
    /// The reply read is bounded by [`CLIENT_QUERY_TIMEOUT`]: a wedged daemon
    /// surfaces as a timeout error rather than parking this thread forever.
    /// The bound is scoped to this exchange, so a later subscription `recv` on
    /// the same client stays unbounded.
    pub fn query(&mut self, request: &serde_json::Value) -> std::io::Result<serde_json::Value> {
        self.send(request)?;
        self.stream.set_read_timeout(Some(CLIENT_QUERY_TIMEOUT))?;
        let received = self.recv();
        let _ = self.stream.set_read_timeout(None);
        received?.ok_or_else(|| std::io::Error::new(ErrorKind::UnexpectedEof, "server closed"))
    }

    /// Send one request frame without waiting for a reply. For a `subscribe`,
    /// whose replies are a stream of frames drained with [`Client::recv`].
    pub fn send(&mut self, request: &serde_json::Value) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(request)?;
        write_frame(&mut self.stream, &bytes)
    }

    /// Read the next frame from the socket, blocking until one arrives. `None`
    /// at a clean EOF.
    pub fn recv(&mut self) -> std::io::Result<Option<serde_json::Value>> {
        match read_frame(&mut self.stream)? {
            Some(frame) => Ok(Some(serde_json::from_slice(&frame)?)),
            None => Ok(None),
        }
    }
}

/// The multi-repo saga over synthetic per-repo plans. No wire driver produces an
/// N>1 mutation yet, so these exercise `apply_saga` directly over temp git repos.
/// Three repos, not two: only N=3 produces a mid-saga failure with more than one
/// member on each side of the compensation split (revert the committed, restore
/// the written-not-committed).
#[cfg(test)]
mod content_read_tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Off a git working tree, `commit` is present-but-null, never absent.
    #[test]
    fn content_at_commit_is_null_off_git() {
        let dir = TempDir::new().unwrap();
        let plain = dir.path().join("note.md");
        fs::write(&plain, "hi\n").unwrap();

        let value = read_content_at(&plain).unwrap();
        assert!(value.get("commit").is_some(), "commit field present");
        assert!(value["commit"].is_null(), "commit null off git");
    }

    /// In a git working tree, `commit` is HEAD of the owning repo.
    #[test]
    fn content_at_commit_is_head_in_a_git_repo() {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "user.name", "Tester"]);
        git(p, &["config", "user.email", "tester@example.com"]);
        fs::write(p.join("note.md"), "hi\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-q", "-m", "seed"]);
        let head = git(p, &["rev-parse", "HEAD"]);

        let plain = read_content_at(&p.join("note.md")).unwrap();
        assert_eq!(plain["commit"].as_str().unwrap(), head);
    }
}

#[cfg(test)]
mod saga_tests {
    use super::*;
    use crate::gitwriter::GitWriteError;
    use crate::repo::RepoName;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    /// A temp git repo with one seed commit, so HEAD exists.
    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        run(p, &["init", "-q", "-b", "main"]);
        run(p, &["config", "user.name", "Tester"]);
        run(p, &["config", "user.email", "tester@example.com"]);
        fs::write(p.join("seed.md"), "seed\n").unwrap();
        run(p, &["add", "."]);
        run(p, &["commit", "-q", "-m", "seed"]);
        dir
    }

    fn run(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A git member touching one path, `f.md`.
    fn member(root: &Path, name: &str) -> SagaMember {
        SagaMember {
            tree_root: root.to_path_buf(),
            repo_names: vec![RepoName(name.to_string())],
            paths: vec![PathBuf::from("f.md")],
            git: true,
        }
    }

    /// The monorepo shape, run through the real grouping: N daemon-owned repos
    /// living in subdirectories of ONE git working tree, each touching its own
    /// `f.md`.
    ///
    /// Deliberately built as the per-repo map `resolve_saga_plan` produces and
    /// then coalesced, so a regression in the grouping shows up here rather than
    /// being assumed away by hand-building the answer.
    fn nested_members(tree: &Path, subs: &[&str]) -> Vec<SagaMember> {
        let mut by_repo: BTreeMap<PathBuf, (RepoName, Vec<PathBuf>)> = BTreeMap::new();
        for sub in subs {
            by_repo.insert(
                tree.join(sub),
                (RepoName(sub.to_string()), vec![tree.join(sub).join("f.md")]),
            );
        }
        coalesce_by_working_tree(by_repo)
    }

    /// Seed a tracked `f.md` under each subdirectory and commit it, so a
    /// later restore-to-HEAD has an original to return to.
    fn seed_nested(tree: &Path, subs: &[&str]) {
        for sub in subs {
            fs::create_dir_all(tree.join(sub)).unwrap();
            fs::write(tree.join(sub).join("f.md"), "seed\n").unwrap();
        }
        run(tree, &["add", "."]);
        run(tree, &["commit", "-q", "-m", "seed nested"]);
    }

    /// How many commits in `tree` carry this `Mutation-Id`.
    fn commits_carrying(tree: &Path, mutation_id: &str) -> usize {
        let log = run(
            tree,
            &[
                "log",
                "--format=%H",
                &format!("--grep=^Mutation-Id: {mutation_id}$"),
            ],
        );
        log.lines().filter(|l| !l.trim().is_empty()).count()
    }

    /// A `GitWriter` that delegates to real git but fails `commit` for one
    /// chosen repo, so a mid-saga failure can be forced while the asserted git
    /// state stays real.
    struct FailAt {
        fail_repo: PathBuf,
    }

    impl GitWriter for FailAt {
        fn commit(
            &self,
            repo: &Path,
            paths: &[PathBuf],
            message: &CommitMessage,
        ) -> Result<CommitSha, GitWriteError> {
            if repo == self.fail_repo {
                return Err(GitWriteError::new("forced commit failure"));
            }
            ShellGit.commit(repo, paths, message)
        }
        fn revert_commit(&self, repo: &Path, id: &MutationId) -> Result<(), GitWriteError> {
            ShellGit.revert_commit(repo, id)
        }
        fn restore_paths(&self, repo: &Path, paths: &[PathBuf]) -> Result<(), GitWriteError> {
            ShellGit.restore_paths(repo, paths)
        }
        fn is_clean(&self, repo: &Path, paths: &[PathBuf]) -> Result<bool, GitWriteError> {
            ShellGit.is_clean(repo, paths)
        }
    }

    #[test]
    fn three_repo_saga_commits_all_with_one_mutation_id() {
        let (a, b, c) = (init_repo(), init_repo(), init_repo());
        let plan = SagaPlan {
            attribution: Vec::new(),
            exempt_from_clean_check: Default::default(),
            mutation_id: "m-saga-1".to_string(),
            members: vec![
                member(a.path(), "ra"),
                member(b.path(), "rb"),
                member(c.path(), "rc"),
            ],
        };
        let roots: Vec<PathBuf> = vec![a.path().into(), b.path().into(), c.path().into()];
        let marker = TempDir::new().unwrap();
        let open_during_write = std::cell::Cell::new(false);
        let (_, commits) = apply_saga(
            &ShellGit,
            marker.path(),
            &plan,
            "test saga".to_string(),
            false,
            || {
                // The marker opens the saga before any tree is touched.
                open_during_write.set(marker.path().join(INTENT_MARKER_NAME).exists());
                for r in &roots {
                    fs::write(r.join("f.md"), "content\n").unwrap();
                }
                Ok::<(), MutationReject>(())
            },
        )
        .expect("the saga commits all members");
        assert_eq!(commits.len(), 3, "one commit per member");
        assert!(
            open_during_write.get(),
            "the intent marker was not open while the trees were written"
        );
        // The intent marker is removed once the saga settles to commit-all.
        assert!(
            !marker.path().join(INTENT_MARKER_NAME).exists(),
            "the intent marker was not closed after a successful saga"
        );
        for (dir, name) in [(&a, "ra"), (&b, "rb"), (&c, "rc")] {
            let body = run(dir.path(), &["log", "-1", "--format=%B"]);
            assert!(
                body.contains("Mutation-Id: m-saga-1"),
                "{name} body: {body}"
            );
            // The member set is identical and name-sorted across every commit.
            assert!(
                body.contains("Mutation-Members: ra, rb, rc"),
                "{name} body: {body}"
            );
            assert!(
                ShellGit
                    .is_clean(dir.path(), &[PathBuf::from("f.md")])
                    .unwrap(),
                "{name} left dirty"
            );
            assert!(dir.path().join("f.md").exists(), "{name} missing f.md");
        }
    }

    #[test]
    fn three_repo_saga_failure_compensates_every_member() {
        let (a, b, c) = (init_repo(), init_repo(), init_repo());
        let plan = SagaPlan {
            attribution: Vec::new(),
            exempt_from_clean_check: Default::default(),
            mutation_id: "m-saga-2".to_string(),
            members: vec![
                member(a.path(), "ra"),
                member(b.path(), "rb"),
                member(c.path(), "rc"),
            ],
        };
        let roots: Vec<PathBuf> = vec![a.path().into(), b.path().into(), c.path().into()];
        // Fail the middle member's commit: ra commits, rb fails, rc is written
        // but never reached for commit.
        let git = FailAt {
            fail_repo: b.path().to_path_buf(),
        };
        let marker = TempDir::new().unwrap();
        let result = apply_saga(
            &git,
            marker.path(),
            &plan,
            "test saga".to_string(),
            false,
            || {
                for r in &roots {
                    fs::write(r.join("f.md"), "content\n").unwrap();
                }
                Ok::<(), MutationReject>(())
            },
        );
        assert!(
            result.is_err(),
            "the saga rejects on the middle member's commit failure"
        );
        // The intent marker is removed once the saga settles to compensate-all.
        assert!(
            !marker.path().join(INTENT_MARKER_NAME).exists(),
            "the intent marker was not closed after a compensated saga"
        );
        // Every member is rolled back: the created f.md is gone, the tree clean.
        for (dir, name) in [(&a, "ra"), (&b, "rb"), (&c, "rc")] {
            assert!(
                !dir.path().join("f.md").exists(),
                "{name}: f.md survived compensation"
            );
            assert!(
                ShellGit
                    .is_clean(dir.path(), &[PathBuf::from("f.md")])
                    .unwrap(),
                "{name}: left dirty after compensation"
            );
        }
        // ra committed, so it is compensated by a revert, not a path-restore.
        let a_log = run(a.path(), &["log", "--format=%s"]);
        assert!(
            a_log.contains("Revert"),
            "repoA's commit was not reverted by Mutation-Id: {a_log}"
        );
        // rb and rc never committed, so the revert is unique to ra.
        let c_log = run(c.path(), &["log", "--format=%s"]);
        assert!(
            !c_log.contains("Revert") && !c_log.contains("test saga"),
            "repoC should carry no mutation commit: {c_log}"
        );
    }

    /// Several daemon-owned repos inside ONE git working tree make ONE commit.
    ///
    /// Git commits per working tree, and one commit per accepted mutation is a
    /// property of this write path, so the saga's member is the tree and the
    /// repos inside it coalesce. N commits carrying one `Mutation-Id` would
    /// break the compensation contract below.
    #[test]
    fn a_shared_working_tree_takes_exactly_one_commit() {
        let tree = init_repo();
        seed_nested(tree.path(), &["repo-a", "repo-b", "repo-c"]);
        let plan = SagaPlan {
            attribution: Vec::new(),
            exempt_from_clean_check: Default::default(),
            mutation_id: "m-shared-1".to_string(),
            members: nested_members(tree.path(), &["repo-a", "repo-b", "repo-c"]),
        };
        let subs = ["repo-a", "repo-b", "repo-c"];
        let marker = TempDir::new().unwrap();
        let (_, commits) = apply_saga(
            &ShellGit,
            marker.path(),
            &plan,
            "test shared tree".to_string(),
            false,
            || {
                for sub in subs {
                    fs::write(tree.path().join(sub).join("f.md"), "mutated\n").unwrap();
                }
                Ok::<(), MutationReject>(())
            },
        )
        .expect("the saga commits");

        assert_eq!(
            commits_carrying(tree.path(), "m-shared-1"),
            1,
            "one mutation must leave one commit in one working tree"
        );
        // The wire serves `commits: { [repo]: sha }`, so each coalesced repo is
        // reported and they SHARE the one sha: a consumer that touched repo-b
        // still learns which commit carries its change.
        assert_eq!(commits.len(), 3, "every covered repo is reported");
        let shas: std::collections::BTreeSet<_> =
            commits.iter().map(|(_, s)| s.0.clone()).collect();
        assert_eq!(shas.len(), 1, "the covered repos must share one sha");
        let names: Vec<&str> = commits.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["repo-a", "repo-b", "repo-c"], "name-sorted");

        // The trailer names every covered repo, name-sorted. Member order is
        // tree-ordered, an internal detail, so a reader must not see it.
        let body = run(tree.path(), &["log", "-1", "--format=%B"]);
        let listed: Vec<&str> = body
            .lines()
            .find_map(|l| l.strip_prefix("Mutation-Members: "))
            .expect("the commit carries a Members trailer")
            .split(", ")
            .collect();
        assert_eq!(listed, ["repo-a", "repo-b", "repo-c"]);
    }

    /// A nested repo's move records its `Moved:` trailer against the enclosing
    /// tree, with tree-relative paths.
    ///
    /// `settle_saga` looks moves up by the member's `tree_root`. Keyed by the
    /// repo root, a nested repo's rename would find no entry and commit with NO
    /// trailer: the forward trace would lose the edge with nothing failing.
    #[test]
    fn a_nested_repos_move_is_keyed_and_rebased_onto_its_tree() {
        let tree = init_repo();
        seed_nested(tree.path(), &["repo-a"]);
        let repo_root = tree.path().join("repo-a");

        let moves = moves_for(
            &repo_root,
            &PathBuf::from("old.md"),
            &PathBuf::from("new.md"),
        );

        let recorded = moves
            .get(tree.path())
            .expect("keyed by the enclosing tree, not the repo root");
        assert_eq!(recorded[0].from, PathBuf::from("repo-a/old.md"));
        assert_eq!(recorded[0].to, PathBuf::from("repo-a/new.md"));
        assert!(
            !moves.contains_key(&repo_root),
            "the repo root must not key the record, settle_saga would miss it"
        );
    }

    /// A repo that IS its own working tree keeps repo-relative paths, so the
    /// common single-repo case is unchanged by the rebasing.
    #[test]
    fn a_standalone_repos_move_is_unchanged_by_the_rebase() {
        let repo = init_repo();
        let moves = moves_for(
            repo.path(),
            &PathBuf::from("old.md"),
            &PathBuf::from("new.md"),
        );
        let recorded = moves.get(repo.path()).expect("keyed by its own root");
        assert_eq!(recorded[0].from, PathBuf::from("old.md"));
        assert_eq!(recorded[0].to, PathBuf::from("new.md"));
    }

    /// Reverting a shared tree's ONE commit restores every repo it covered.
    ///
    /// Compensation addresses a mutation's commit by its `Mutation-Id`, and
    /// `find_mutation_sha` REFUSES when more than one commit carries it. If the
    /// coalescing regressed to a member per repo, one mutation would land N
    /// commits in this tree and compensation would fail outright rather than
    /// half-apply — loud, but still a broken rollback this guards against.
    ///
    /// Forced by failing a SECOND tree's commit, after the shared tree has
    /// committed: with coalescing there is no "second commit into one tree" to
    /// fail, which is the property under test.
    #[test]
    fn a_shared_tree_failure_restores_every_coalesced_path() {
        let tree = init_repo();
        seed_nested(tree.path(), &["repo-a", "repo-b", "repo-c"]);
        let other = init_repo();
        let subs = ["repo-a", "repo-b", "repo-c"];

        let mut members = nested_members(tree.path(), &subs);
        members.push(member(other.path(), "other"));
        let plan = SagaPlan {
            attribution: Vec::new(),
            exempt_from_clean_check: Default::default(),
            mutation_id: "m-shared-2".to_string(),
            members,
        };
        // The shared tree commits first (tree-root ordered), then this fails.
        let git = FailAt {
            fail_repo: other.path().to_path_buf(),
        };
        let marker = TempDir::new().unwrap();
        let result = apply_saga(
            &git,
            marker.path(),
            &plan,
            "test shared tree".to_string(),
            false,
            || {
                for sub in subs {
                    fs::write(tree.path().join(sub).join("f.md"), "mutated\n").unwrap();
                }
                fs::write(other.path().join("f.md"), "mutated\n").unwrap();
                Ok::<(), MutationReject>(())
            },
        );
        assert!(result.is_err(), "the saga rejects on the commit failure");

        // Every coalesced repo is back at its seeded content, none left applied
        // by a revert that could only reach one commit.
        for sub in subs {
            let body = fs::read_to_string(tree.path().join(sub).join("f.md")).unwrap();
            assert_eq!(
                body, "seed\n",
                "{sub}: not restored, a coalesced repo survived compensation"
            );
        }
        assert!(
            ShellGit
                .is_clean(
                    tree.path(),
                    &subs
                        .iter()
                        .map(|s| PathBuf::from(s).join("f.md"))
                        .collect::<Vec<_>>()
                )
                .unwrap(),
            "the shared tree is left dirty after compensation"
        );
    }

    #[test]
    fn dirty_set_spans_every_member_path() {
        let plan = SagaPlan {
            attribution: Vec::new(),
            exempt_from_clean_check: Default::default(),
            mutation_id: "m".to_string(),
            members: vec![
                SagaMember {
                    tree_root: PathBuf::from("/a"),
                    repo_names: vec![RepoName("ra".to_string())],
                    paths: vec![PathBuf::from("x.md"), PathBuf::from("y.md")],
                    git: true,
                },
                SagaMember {
                    tree_root: PathBuf::from("/b"),
                    repo_names: vec![RepoName("rb".to_string())],
                    paths: vec![PathBuf::from("z.md")],
                    git: false,
                },
            ],
        };
        let dirty = saga_dirty_set(&plan);
        // One rebuild over the union: every member's path, so the version bumps
        // once after settle rather than once per member.
        assert_eq!(dirty.len(), 3, "dirty set: {dirty:?}");
        assert!(dirty.contains(&PathBuf::from("/a/x.md")));
        assert!(dirty.contains(&PathBuf::from("/a/y.md")));
        // A non-git member's written file is still re-read into the IR.
        assert!(
            dirty.contains(&PathBuf::from("/b/z.md")),
            "dirty set: {dirty:?}"
        );
    }

    /// Write the intent marker for a crashed saga over `members` (each touching
    /// `f.md`), as the live saga would have before touching the trees.
    fn crashed_marker(marker_root: &Path, id: &str, members: &[(&Path, &str)]) {
        let plan = SagaPlan {
            attribution: Vec::new(),
            exempt_from_clean_check: Default::default(),
            mutation_id: id.to_string(),
            members: members.iter().map(|(p, n)| member(p, n)).collect(),
        };
        write_intent_marker(
            &marker_root.join(".arsumbris").join("au-engine").join("run"),
            &plan,
        )
        .unwrap();
    }

    /// A crashed saga over ONE working tree holding several repos recovers by
    /// reverting its single commit, restoring every repo the tree covered.
    ///
    /// The marker records one line per TREE, so recovery classifies once and the
    /// one revert reaches every coalesced repo. A marker written per repo would
    /// classify the same tree N times and revert its one commit repeatedly.
    #[test]
    fn recovery_over_a_shared_tree_restores_every_coalesced_repo() {
        let ws = TempDir::new().unwrap();
        let tree = init_repo();
        let subs = ["repo-a", "repo-b", "repo-c"];
        seed_nested(tree.path(), &subs);

        // The saga committed its one coalesced commit, then crashed before
        // closing the marker.
        for sub in subs {
            fs::write(tree.path().join(sub).join("f.md"), "mutated\n").unwrap();
        }
        let paths: Vec<PathBuf> = subs.iter().map(|s| PathBuf::from(s).join("f.md")).collect();
        let message = CommitMessage {
            summary: "crashed shared-tree mutation".to_string(),
            mutation_id: MutationId("m-crash-shared".to_string()),
            members: subs.iter().map(|s| RepoName(s.to_string())).collect(),
            moves: Vec::new(),
            reverts: None,
            attribution: Vec::new(),
        };
        ShellGit.commit(tree.path(), &paths, &message).unwrap();

        let plan = SagaPlan {
            attribution: Vec::new(),
            exempt_from_clean_check: Default::default(),
            mutation_id: "m-crash-shared".to_string(),
            members: nested_members(tree.path(), &subs),
        };
        write_intent_marker(
            &ws.path().join(".arsumbris").join("au-engine").join("run"),
            &plan,
        )
        .unwrap();

        let report = recover_crashed_saga(ws.path());
        let recovered = report.recovered.expect("the open marker is recovered");
        assert_eq!(
            recovered.reverted,
            vec![tree.path().to_path_buf()],
            "one tree, one revert, not one per covered repo"
        );
        assert!(
            report.failures.is_empty(),
            "recovery reported failures: {:?}",
            report.failures
        );
        for sub in subs {
            let body = fs::read_to_string(tree.path().join(sub).join("f.md")).unwrap();
            assert_eq!(body, "seed\n", "{sub}: not restored by the single revert");
        }
    }

    /// A committed mutation write of `f.md` carrying `id`'s trailer.
    fn commit_mutation(repo: &Path, id: &str) {
        fs::write(repo.join("f.md"), "mutated\n").unwrap();
        let message = CommitMessage {
            summary: "crashed mutation".to_string(),
            mutation_id: MutationId(id.to_string()),
            members: vec![RepoName("m".to_string())],
            moves: Vec::new(),
            reverts: None,
            attribution: Vec::new(),
        };
        ShellGit
            .commit(repo, &[PathBuf::from("f.md")], &message)
            .unwrap();
    }

    #[test]
    fn recovery_is_a_noop_without_a_marker() {
        let ws = TempDir::new().unwrap();
        let report = recover_crashed_saga(ws.path());
        assert!(report.recovered.is_none(), "a clean start finds no marker");
        assert!(report.failures.is_empty(), "and no failure sentinels");
    }

    #[test]
    fn recovery_reverts_committed_and_restores_written_members() {
        let ws = TempDir::new().unwrap();
        let (ra, rb, rc) = (init_repo(), init_repo(), init_repo());

        // The saga m-crash committed ra, then crashed before committing rb / rc,
        // which are written but not committed.
        commit_mutation(ra.path(), "m-crash");
        fs::write(rb.path().join("f.md"), "mutated\n").unwrap();
        fs::write(rc.path().join("f.md"), "mutated\n").unwrap();
        crashed_marker(
            ws.path(),
            "m-crash",
            &[(ra.path(), "ra"), (rb.path(), "rb"), (rc.path(), "rc")],
        );

        let report = recover_crashed_saga(ws.path());
        assert!(
            report.failures.is_empty(),
            "a clean recovery has no failures"
        );
        let recovery = report.recovered.expect("a marker was present");
        assert_eq!(recovery.mutation_id, "m-crash");
        assert_eq!(
            recovery.reverted.len(),
            1,
            "ra had committed, so it reverts"
        );
        assert_eq!(recovery.restored.len(), 2, "rb and rc are path-restored");

        // ra's committed f.md is reverted away; rb and rc's dirty f.md is removed.
        for r in [&ra, &rb, &rc] {
            assert!(
                !r.path().join("f.md").exists(),
                "f.md should be gone after recovery"
            );
            assert!(
                ShellGit
                    .is_clean(r.path(), &[PathBuf::from("f.md")])
                    .unwrap(),
                "tree not clean after recovery"
            );
        }
        // The marker is cleared, so a second start is a no-op.
        assert!(
            !ws.path()
                .join(".arsumbris")
                .join("au-engine")
                .join("run")
                .join(INTENT_MARKER_NAME)
                .exists(),
            "the marker was not cleared"
        );
    }

    #[test]
    fn recovery_rolls_back_a_crash_before_the_first_commit() {
        let ws = TempDir::new().unwrap();
        let (ra, rb) = (init_repo(), init_repo());

        // Crash after writing the trees but before any commit: zero trailers, the
        // marker is the only signal these dirty trees are a crashed mutation.
        fs::write(ra.path().join("f.md"), "mutated\n").unwrap();
        fs::write(rb.path().join("f.md"), "mutated\n").unwrap();
        crashed_marker(
            ws.path(),
            "m-crash2",
            &[(ra.path(), "ra"), (rb.path(), "rb")],
        );

        let recovery = recover_crashed_saga(ws.path())
            .recovered
            .expect("a marker was present");
        assert!(
            recovery.reverted.is_empty(),
            "nothing committed, nothing to revert"
        );
        assert_eq!(recovery.restored.len(), 2, "both written members restored");
        assert!(!ra.path().join("f.md").exists(), "ra not rolled back");
        assert!(!rb.path().join("f.md").exists(), "rb not rolled back");
        assert!(
            !ws.path()
                .join(".arsumbris")
                .join("au-engine")
                .join("run")
                .join(INTENT_MARKER_NAME)
                .exists(),
            "the marker was not cleared"
        );
    }

    #[test]
    fn recovery_with_a_conflicting_revert_leaves_a_failure_sentinel() {
        let ws = TempDir::new().unwrap();
        let ra = init_repo();

        // The saga m-confl committed f.md in ra, then crashed.
        commit_mutation(ra.path(), "m-confl");
        // A human committed over the engine's commit, changing f.md, so reverting
        // m-confl will conflict.
        fs::write(ra.path().join("f.md"), "human edit\n").unwrap();
        run(ra.path(), &["add", "f.md"]);
        run(ra.path(), &["commit", "-q", "-m", "human edit"]);
        crashed_marker(ws.path(), "m-confl", &[(ra.path(), "ra")]);

        let report = recover_crashed_saga(ws.path());
        // The intent marker was processed and removed.
        assert!(report.recovered.is_some(), "the marker was processed");
        assert!(
            !ws.path()
                .join(".arsumbris")
                .join("au-engine")
                .join("run")
                .join(INTENT_MARKER_NAME)
                .exists(),
            "the intent marker was not cleared"
        );
        // The conflicted revert was recorded durably, not silently dropped.
        assert_eq!(
            report.failures.len(),
            1,
            "the failure must surface: {report:?}"
        );
        assert_eq!(report.failures[0].mutation_id, "m-confl");
        assert!(
            ws.path()
                .join(".arsumbris")
                .join("au-engine")
                .join("run")
                .join("saga.failed.m-confl")
                .exists(),
            "the failure sentinel must be on disk"
        );

        // A restart re-surfaces the sentinel and finds no marker.
        let again = recover_crashed_saga(ws.path());
        assert!(
            again.recovered.is_none(),
            "no intent marker the second time"
        );
        assert_eq!(again.failures.len(), 1, "the sentinel is re-surfaced");
    }
}
