//! The hardwired `au.engine.*` schema type-defs.
//!
//! Every engine-schema file is a typed instance of one of these compiled-in
//! defs, owned by a reserved `au-engine` repo identity, never authored in a
//! knowledge base: the in-repo `repo.yaml`, the per-user `repos.yaml` / `workspaces.yaml`,
//! the in-repo `.arsumbris/workspace.yaml`, and the `.arsumbris/` locks. The
//! type-def IS the schema, self-documenting and `(name, hash)`-comparable like
//! any cross-repo type. See
//! [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]]
//! "Engine files are typed" and
//! [[spec - engine-schema files - hardwired-schema files are first-class substrate nodes]].
//!
//! The defs are authored as ordinary `.type.yaml` source and parsed through the
//! real `parse_file` path, so their `Shape` ASTs, spans, and canonical hashes
//! come out exactly as a repo-authored def's would. That keeps them
//! wire-serializable (the shape-AST read) and drift-comparable against a
//! consumer's mirror, both without a rework.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::LazyLock;

use au_core::TypeDef;
use au_diagnostics::{ByteRange, Diagnostic, DiagnosticCode, Severity, Span};

use crate::parse::{parse_file, FileParse};
use crate::repo::{Repo, RepoName};

/// The reserved repo identity owning every hardwired engine-schema def.
///
/// A knowledge base's same-named def coexists as a DISTINCT `(name, hash)` identity, they
/// compose like any cross-repo pair, `::au-engine` scopes the engine's. This is
/// the engine's slice of the layered-ownership pattern (host owns `au.host.*`,
/// harness owns `au.mcp.*`).
pub const BUILTIN_ENGINE_REPO: &str = "au-engine";

/// The nominal meta marker type-name, `au.engine.meta`. A type is legal in a
/// `meta:` position only if its resolved closure mixes this in (as the qualified
/// `au.engine.meta::au-engine`). Injected into au-core's validator so it stays
/// domain-pure. See
/// [[spec - meta type marker - the meta position admits only types that mix in the engine meta base]].
pub const ENGINE_META_TYPE: &str = "au.engine.meta";

/// The reserved type-name namespace. A def named `au.engine` or under
/// `au.engine.` sits in the engine's reserved namespace, see
/// [`is_engine_namespace`].
const ENGINE_NAMESPACE: &str = "au.engine";

/// A knowledge base type-def in the reserved `au.engine.*` namespace whose name the
/// engine does NOT hardwire. The namespace is reserved forward: a future engine
/// version may hardwire this name, and the knowledge base def would then live-shadow it.
/// Advisory (`hint`), never blocks; the knowledge base def keeps its own identity and
/// resolves normally. See
/// [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]]
/// "Engine files are typed".
pub const ENGINE_NAME_FORWARD_RESERVED: DiagnosticCode =
    DiagnosticCode::from_static("engine-name-forward-reserved");

/// A knowledge base type-def named exactly like a hardwired `au.engine.*` type. The two
/// COEXIST as distinct `(name, hash)` identities, `::au-engine` scopes the
/// engine's; the code only NOTES the engine owns the name, it never ignores the
/// knowledge base's def. Advisory (`warning`), never blocks. See
/// [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]]
/// "Engine files are typed".
pub const ENGINE_NAME_LIVE_SHADOW: DiagnosticCode =
    DiagnosticCode::from_static("engine-name-live-shadow");

/// A knowledge base type-def that shadows a hardwired `au.engine.*` type AND has DIVERGED
/// from it: its `(name, closure-hash)` identity differs from the engine's copy.
/// Advisory `drift`, the same tier as a drifted pinned reference, never blocks.
/// The base coexistence is `engine-name-live-shadow`; this escalates a diverged
/// shadow so a consumer can rank it above an in-sync one (which fires no drift).
pub const ENGINE_NAME_SHADOW_DRIFT: DiagnosticCode =
    DiagnosticCode::from_static("engine-name-shadow-drift");

/// An engine-schema file carries no written `type:` key. Its kind still assigns
/// the FLOOR type, so the file stays fully correct and the engine reads its data
/// regardless. Advisory `drift`, a high-attention nudge to self-describe: the
/// engine's own writers emit the qualified `type: au.engine.X::au-engine`, so an
/// absent one reads as a hand-authored or legacy file. Never blocks. See
/// [[spec - engine-schema file claims - the kind assigns a floor, a written type self-describes and mixes in more]].
pub const ENGINE_SCHEMA_TYPE_UNWRITTEN: DiagnosticCode =
    DiagnosticCode::from_static("engine-schema-type-unwritten");

/// An engine-schema file writes a `type:` whose resolved closure OMITS the
/// kind's floor type (e.g. a `repo.yaml` written `type: au.engine.workspace::au-engine`).
/// The written claim names the wrong kind, so the file does not self-describe as
/// what it structurally is. An `error`; the engine still reads its data from the
/// kind-assigned floor (the resolution path is claim-independent), so it is
/// recoverable, never a lost-data case. A written claim that INCLUDES the floor
/// and mixes in more is fine. See
/// [[spec - engine-schema file claims - the kind assigns a floor, a written type self-describes and mixes in more]].
pub const ENGINE_SCHEMA_TYPE_FLOOR_OMITTED: DiagnosticCode =
    DiagnosticCode::from_static("engine-schema-type-floor-omitted");

/// Whether a type name sits in the reserved `au.engine.*` namespace: the exact
/// root `au.engine`, or any dotted descendant `au.engine.<leaf>`. The dot guard
/// keeps an unrelated name like `au.engineering` out of the namespace.
pub(crate) fn is_engine_namespace(name: &str) -> bool {
    name == ENGINE_NAMESPACE || name.starts_with("au.engine.")
}

/// Whether a name is one of the hardwired engine-schema defs.
pub(crate) fn is_hardwired_engine_name(name: &str) -> bool {
    DEFS.iter().any(|(n, _)| *n == name)
}

/// The coexistence note for a REPO-authored type-def whose name sits in the
/// reserved `au.engine.*` namespace, or `None` for an ordinary name.
///
/// The engine reserves the namespace by CONVENTION, not annihilation: the knowledge base
/// def keeps its own `(name, hash)` identity and resolves like any other, so the
/// note is advisory. A hardwired name is `engine-name-live-shadow` (both
/// coexist, `::au-engine` scopes the engine's); any other reserved name is
/// `engine-name-forward-reserved` (the engine may claim it later). The builtin's
/// own defs never reach this: they are seeded straight into the `au-engine`
/// repo, never walked as knowledge base files.
pub(crate) fn engine_name_reservation_diag(td: &TypeDef) -> Option<Diagnostic> {
    let name = td.name.0.as_str();
    if !is_engine_namespace(name) {
        return None;
    }
    let span = Span::new(td.source_path.clone(), td.source_span);
    let diag = if is_hardwired_engine_name(name) {
        Diagnostic {
            code: ENGINE_NAME_LIVE_SHADOW,
            severity: Severity::Warning,
            span,
            message: format!(
                "type-def '{name}' shares a name with a hardwired engine type; both coexist as \
                 distinct identities, '{name}::au-engine' names the engine's"
            ),
            related: vec![],
            fix: None,
        }
    } else {
        Diagnostic {
            code: ENGINE_NAME_FORWARD_RESERVED,
            severity: Severity::Hint,
            span,
            message: format!(
                "type-def '{name}' is in the engine-reserved 'au.engine.*' namespace, which the \
                 engine may hardwire in a future version"
            ),
            related: vec![],
            fix: None,
        }
    };
    Some(diag)
}

/// The drift note for a hardwired-name shadow whose closure-hash has DIVERGED
/// from the engine's copy. The caller compares the two `(name, closure-hash)`
/// identities post-graph-build and calls this only on a divergence; an in-sync
/// shadow (equal hashes) is silent, its coexistence already noted by
/// `engine-name-live-shadow`.
pub(crate) fn engine_name_shadow_drift_diag(
    name: &str,
    file: PathBuf,
    span: ByteRange,
) -> Diagnostic {
    Diagnostic {
        code: ENGINE_NAME_SHADOW_DRIFT,
        severity: Severity::Drift,
        span: Span::new(file, span),
        message: format!(
            "type-def '{name}' shadows a hardwired engine type and has DIVERGED from it; the \
             engine's copy is '{name}::au-engine'"
        ),
        related: vec![],
        fix: None,
    }
}

/// A sentinel root for the builtin repo, NOT an on-disk path.
///
/// No walked file sits under it, so `RepoMap::repo_of` never routes a real file
/// to the builtin repo, and `RepoMap::root()` skips it via the `builtin` flag.
/// The builtin's defs are seeded straight into its graph, never read from disk.
pub(crate) const BUILTIN_ROOT: &str = "<au-engine-builtin>";

/// The hardwired defs as `(type-name, source)`. The source is the schema:
/// seven file types, the record element types their list fields reference, and
/// the `au.engine.meta` marker base. Every field is a primitive, a `String[]`, or
/// a `T[]` of one of these element records, so the whole set is expressible with
/// no maps. The `au.engine.readme` file type is body-only, a self-declared
/// markdown node with a section template and no fields. The marker base is a
/// fieldless abstract tag, see
/// [[spec - meta type marker - the meta position admits only types that mix in the engine meta base]].
const DEFS: &[(&str, &str)] = &[
    // --- File types ----------------------------------------------------------
    (
        "au.engine.repo",
        "\
#: A repo's committed identity and dependency set, `.arsumbris/repo.yaml`.
fields:
  name: String
  remote?: String
  description?: String
  deps?: au.engine.dep[]
",
    ),
    (
        "au.engine.repos",
        "\
#: The per-user registry, `~/.arsumbris/au-engine/config/repos.yaml`, name to remote+path.
fields:
  repos?: au.engine.registry-entry[]
",
    ),
    (
        "au.engine.workspaces",
        "\
#: The per-user workspaces index, `~/.arsumbris/au-engine/config/workspaces.yaml`.
fields:
  workspaces?: au.engine.workspace-entry[]
",
    ),
    (
        "au.engine.workspace",
        "\
#: The optional workspace composition, `.arsumbris/workspace.yaml`, edit + discover members. `disabled:` overlays the role lists, a member declared but intentionally not mounted.
fields:
  edit?: String[]
  discover?: String[]
  disabled?: String[]
",
    ),
    (
        "au.engine.repo-lock",
        "\
#: The per-repo dependency lock, `.arsumbris/repo.lock`, engine-written.
fields:
  packages?: au.engine.locked-package[]
",
    ),
    (
        "au.engine.workspace-lock",
        "\
#: The per-workspace discover lock, `.arsumbris/workspace.lock`, engine-written.
fields:
  packages?: au.engine.locked-package[]
",
    ),
    (
        "au.engine.readme",
        "\
#: A repo's self-description at its root, `README.md`. Self-declared, its
#: sections say what the repo is, how to use it, and how to extend it.
fields:
  #: A one-line digest of the whole README, about one sentence per section:
  #: what the repo is, how to use it, how to extend it. E.g. \"Engine over a
  #: typed file graph. Use it via the daemon. Build on top by defining your
  #: own types.\"
  tldr: String
body:
  - section: Repo Overview
    guidance: \"The engine-owned self-description block. Keep the repo's own top-level sections outside it.\"
    body:
      - section: What this is
        guidance: \"What this repo is and the role it plays. Name the domain and what it holds, not its history.\"
      - section: How to use this
        guidance: \"How a human or agent consumes this repo. The entry points, the main commands or reads, and where to start.\"
      - section: How to extend this
        guidance: \"How a downstream repo builds on this one's types. Which types it exposes for a consumer to import via `::repo`, subtype, or claim as an instance identity, and the conventions for doing so.\"
",
    ),
    // --- Element records -----------------------------------------------------
    (
        "au.engine.dep",
        "\
#: One dependency entry in a repo's `deps`.
fields:
  name: String
  remote?: String
  ref?: String
",
    ),
    (
        "au.engine.registry-entry",
        "\
#: One entry in the per-user registry: a name's remote and local path.
fields:
  name: String
  remote?: String
  path: String
",
    ),
    (
        "au.engine.workspace-entry",
        "\
#: One entry in the workspaces index: a workspace file and an optional alias.
fields:
  path: String
  name?: String
",
    ),
    (
        "au.engine.locked-package",
        "\
#: One pinned dependency in a repo lock: a name, its remote, and the sha.
fields:
  name: String
  remote: String
  sha: String
",
    ),
    // --- Marker bases --------------------------------------------------------
    (
        "au.engine.meta",
        "\
#: The nominal meta marker. A type-def is legal in a `meta:` position only if
#: its resolved closure mixes this in, via `au.engine.meta::au-engine`. Abstract,
#: never claimed directly, a block names a concrete subtype that carries it.
abstract: true
fields: {}
",
    ),
];

/// The hardwired `au.engine.*` type-defs, parsed once through the real parse
/// path. A parse error or a missing def is a programming error in a compiled-in
/// source string, so it panics loudly rather than silently dropping a def.
static BUILTIN_DEFS: LazyLock<Vec<TypeDef>> = LazyLock::new(|| {
    DEFS.iter()
        .map(|(name, src)| {
            let path = PathBuf::from(BUILTIN_ROOT).join(format!("{name}.type.yaml"));
            match parse_file(&path, src.as_bytes()) {
                FileParse::TypeDef {
                    type_def: Some(td),
                    diagnostics,
                    ..
                } => {
                    assert!(
                        diagnostics.iter().all(|d| d.severity != Severity::Error),
                        "hardwired engine schema '{name}' has parse errors: {diagnostics:?}"
                    );
                    assert_eq!(
                        td.name.0, *name,
                        "hardwired engine schema path-derived name mismatch"
                    );
                    td
                }
                other => panic!(
                    "hardwired engine schema '{name}' did not parse to a type-def: {other:?}"
                ),
            }
        })
        .collect()
});

/// The hardwired `au.engine.*` type-defs, cloned from the memoized parse.
pub(crate) fn builtin_engine_defs() -> Vec<TypeDef> {
    BUILTIN_DEFS.clone()
}

/// The compiled-in `au-engine` repo: a fixed identity with no on-disk root,
/// present in every built knowledge base so `::au-engine` resolves as a universal peer
/// (the resolution gate finds it by name, no `deps` declaration needed).
pub(crate) fn builtin_engine_repo() -> Repo {
    Repo {
        root: PathBuf::from(BUILTIN_ROOT),
        name: RepoName(BUILTIN_ENGINE_REPO.to_string()),
        declared: true,
        builtin: true,
        description: None,
        remote: None,
        deps: Vec::new(),
        peer_paths: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use au_core::{
        build_graph, run_body_typing_checks, run_graph_structure_checks, run_inheritance_checks,
    };

    /// Exactly the finalized set: seven file types, four element records, and the
    /// `au.engine.meta` marker base.
    const EXPECTED: &[&str] = &[
        "au.engine.repo",
        "au.engine.repos",
        "au.engine.workspaces",
        "au.engine.workspace",
        "au.engine.repo-lock",
        "au.engine.workspace-lock",
        "au.engine.readme",
        "au.engine.dep",
        "au.engine.registry-entry",
        "au.engine.workspace-entry",
        "au.engine.locked-package",
        "au.engine.meta",
    ];

    #[test]
    fn the_hardwired_set_is_exactly_the_finalized_leaf_names() {
        let defs = builtin_engine_defs();
        let mut names: Vec<&str> = defs.iter().map(|d| d.name.0.as_str()).collect();
        names.sort_unstable();
        let mut expected: Vec<&str> = EXPECTED.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected);
    }

    #[test]
    fn the_hardwired_defs_build_a_clean_graph_with_identities() {
        // The whole set feeds one graph, as it does under the `au-engine` repo
        // at build time. Every element ref resolves within the set, so the load
        // checks must be silent — a hardwired schema is authoritative.
        let build = build_graph(builtin_engine_defs());
        let mut diags = build.diagnostics;
        diags.extend(run_graph_structure_checks(&build.graph));
        diags.extend(run_inheritance_checks(&build.graph));
        diags.extend(run_body_typing_checks(&build.graph));
        diags.extend(au_core::run_location_checks(&build.graph, None, None));
        assert!(
            diags.iter().all(|d| d.severity != Severity::Error),
            "hardwired engine schema graph has errors: {diags:?}"
        );
        // Identity is `(name, closure-hash)`; every def gets a closure id, so the
        // set is drift-comparable against a consumer mirror (Action 6).
        for name in EXPECTED {
            assert!(
                build
                    .graph
                    .closure_id(&au_core::TypeName(name.to_string()))
                    .is_some(),
                "no closure id computed for hardwired '{name}'"
            );
        }
    }

    #[test]
    fn the_builtin_repo_is_flagged_and_named() {
        let repo = builtin_engine_repo();
        assert!(repo.builtin);
        assert_eq!(repo.name.as_str(), BUILTIN_ENGINE_REPO);
        assert!(repo.deps.is_empty());
    }

    #[test]
    fn engine_namespace_membership() {
        assert!(is_engine_namespace("au.engine"));
        assert!(is_engine_namespace("au.engine.repo"));
        assert!(is_engine_namespace("au.engine.custom-thing"));
        // The dot guard keeps a merely-prefixed name out of the namespace.
        assert!(!is_engine_namespace("au.engineering"));
        assert!(!is_engine_namespace("engine"));
        assert!(!is_engine_namespace("note"));
    }

    #[test]
    fn hardwired_name_membership() {
        assert!(is_hardwired_engine_name("au.engine.repo"));
        assert!(is_hardwired_engine_name("au.engine.locked-package"));
        // In the namespace, but not one of the hardwired defs.
        assert!(!is_hardwired_engine_name("au.engine.widget"));
        assert!(!is_hardwired_engine_name("note"));
    }

    fn td_named(name: &str) -> TypeDef {
        let path = PathBuf::from(format!("{name}.type.yaml"));
        match parse_file(&path, b"fields:\n  x: String\n") {
            FileParse::TypeDef {
                type_def: Some(td), ..
            } => td,
            other => panic!("'{name}' did not parse to a type-def: {other:?}"),
        }
    }

    #[test]
    fn a_hardwired_name_in_the_repo_is_live_shadow_warning() {
        let td = td_named("au.engine.repo");
        let d = engine_name_reservation_diag(&td).expect("a reserved name fires");
        assert_eq!(d.code, ENGINE_NAME_LIVE_SHADOW);
        assert_eq!(d.severity, Severity::Warning);
    }

    #[test]
    fn a_reserved_but_unhardwired_name_is_forward_reserved_hint() {
        let td = td_named("au.engine.widget");
        let d = engine_name_reservation_diag(&td).expect("a reserved name fires");
        assert_eq!(d.code, ENGINE_NAME_FORWARD_RESERVED);
        assert_eq!(d.severity, Severity::Hint);
    }

    #[test]
    fn an_ordinary_name_fires_no_reservation_note() {
        let td = td_named("note");
        assert!(engine_name_reservation_diag(&td).is_none());
    }
}
