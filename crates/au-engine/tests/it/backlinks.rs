//! The reverse reference index, over a fixture scenario and a mutable repo.

use std::fs;
use std::path::{Path, PathBuf};

use au_engine::{build, Backlink, RefSurface};
use au_parser::RealFileSystem;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Find the inbound edges for the target whose file name matches `name`,
/// matching by file name to sidestep absolute-path form differences.
fn inbound_for<'a>(
    backlinks: &'a au_engine::OrdMap<PathBuf, Vec<Backlink>>,
    name: &str,
) -> &'a [Backlink] {
    backlinks
        .iter()
        .find(|(p, _)| p.file_name().map(|n| n == name).unwrap_or(false))
        .map(|(_, edges)| edges.as_slice())
        .unwrap_or(&[])
}

fn has_source_named(edges: &[Backlink], name: &str) -> bool {
    edges
        .iter()
        .any(|e| e.source.file_name().map(|n| n == name).unwrap_or(false))
}

#[test]
fn inbound_set_is_correct() {
    // decision.md body links `[[engine-project:notes]]` and
    // `[[sqlite-followup:notes]]`, both `file*` body references.
    let repo = workspace_root().join("scenarios/body-file-star-clean");
    let v = build(&repo, &RealFileSystem).expect("build");

    let to_engine = inbound_for(&v.backlinks, "engine-project.md");
    assert!(
        has_source_named(to_engine, "decision.md"),
        "engine-project.md has an inbound edge from decision.md, got {to_engine:?}"
    );

    let to_sqlite = inbound_for(&v.backlinks, "sqlite-followup.md");
    assert!(
        has_source_named(to_sqlite, "decision.md"),
        "sqlite-followup.md has an inbound edge from decision.md, got {to_sqlite:?}"
    );

    // The edges are body wikilinks carrying the `:notes` field attribution.
    let edge = to_engine
        .iter()
        .find(|e| e.source.file_name().unwrap() == "decision.md")
        .unwrap();
    assert_eq!(edge.surface, RefSurface::Body);
    assert_eq!(edge.slot.as_deref(), Some("notes"));

    // A file nothing points at has an empty inbound set.
    assert!(
        v.backlinks(&repo.join("decision.md")).is_empty()
            || !has_source_named(v.backlinks(&repo.join("decision.md")), "decision.md"),
        "decision.md is not referenced by these instances"
    );
}

#[test]
fn embedded_wikilink_in_a_string_value_is_an_edge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = fs::canonicalize(dir.path()).expect("canonicalize");
    crate::seed_repo(&root);

    fs::create_dir(root.join("type")).unwrap();
    // `description` is a plain String, not a reference slot.
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  description?: String\n",
    )
    .unwrap();
    fs::write(
        root.join("a.md"),
        "---\ntype: note\ndescription: \"Runs the show at [[b]].\"\n---\n",
    )
    .unwrap();
    fs::write(root.join("b.md"), "---\ntype: note\n---\n").unwrap();

    let v = build(&root, &RealFileSystem).expect("build");

    // The embedded link is a navigational edge even though the slot is a
    // plain String, not a reference type.
    let to_b = inbound_for(&v.backlinks, "b.md");
    assert!(
        has_source_named(to_b, "a.md"),
        "b.md has an inbound edge from a.md via the embedded link, got {to_b:?}"
    );
    let edge = to_b
        .iter()
        .find(|e| e.source.file_name().unwrap() == "a.md")
        .unwrap();
    assert_eq!(edge.surface, RefSurface::Frontmatter);
    assert_eq!(edge.slot.as_deref(), Some("description"));
}

#[test]
fn a_type_def_field_docstring_forms_a_navigational_edge() {
    // The motivating case: a field's `#:` docstring links a doc file in the same
    // repo. It resolves as a navigational edge tagged with the docstring surface,
    // its slot naming the documented field. A type-def has no other edges.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = fs::canonicalize(dir.path()).expect("canonicalize");
    crate::seed_repo(&root);

    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/task.type.yaml"),
        "fields:\n  gate: String   #: see [[gate-policy]]\n",
    )
    .unwrap();
    fs::write(
        root.join("gate-policy.md"),
        "---\ntype: note\n---\nGate policy.\n",
    )
    .unwrap();

    let v = build(&root, &RealFileSystem).expect("build");

    let to_policy = inbound_for(&v.backlinks, "gate-policy.md");
    let edge = to_policy
        .iter()
        .find(|e| e.source.file_name().unwrap() == "task.type.yaml")
        .expect("a docstring edge from the type-def");
    assert_eq!(edge.surface, RefSurface::Docstring);
    assert_eq!(edge.slot.as_deref(), Some("gate"));
}

#[test]
fn an_instance_head_docstring_forms_a_navigational_edge() {
    // A `#:` head docstring above `type:` documents the instance. Its link is a
    // navigational docstring edge with no slot (the head, not a field).
    let dir = tempfile::tempdir().expect("tempdir");
    let root = fs::canonicalize(dir.path()).expect("canonicalize");
    crate::seed_repo(&root);

    fs::create_dir(root.join("type")).unwrap();
    fs::write(root.join("type/note.type.yaml"), "fields:\n  x?: String\n").unwrap();
    fs::write(
        root.join("a.md"),
        "---\n#: motivated by [[b]]\ntype: note\n---\n",
    )
    .unwrap();
    fs::write(root.join("b.md"), "---\ntype: note\n---\n").unwrap();

    let v = build(&root, &RealFileSystem).expect("build");

    let to_b = inbound_for(&v.backlinks, "b.md");
    let edge = to_b
        .iter()
        .find(|e| e.source.file_name().unwrap() == "a.md")
        .expect("a head-docstring edge from a.md");
    assert_eq!(edge.surface, RefSurface::Docstring);
    assert_eq!(edge.slot, None);
}

#[test]
fn a_reference_inside_a_body_fence_record_forms_a_backlink() {
    // A reference that is a FIELD VALUE inside a marked record fence indexes
    // like the same record written in frontmatter: a Frontmatter-surface edge
    // keyed by the inner field. Previously these were silently unindexed, so a
    // rename could not rewrite them and would strand the link.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = fs::canonicalize(dir.path()).expect("canonicalize");
    crate::seed_repo(&root);

    fs::create_dir(root.join("type")).unwrap();
    fs::write(root.join("type/step.type.yaml"), "fields:\n  ref: file*\n").unwrap();
    fs::write(root.join("type/host.type.yaml"), "fields:\n  slot: step\n").unwrap();
    fs::write(
        root.join("host.md"),
        "---\ntype: host\nslot:\n---\n\n```yaml [:slot]\ntype: step\nref: \"[[target]]\"\n```\n",
    )
    .unwrap();
    fs::write(root.join("target.md"), "---\ntype: note\n---\n").unwrap();

    let v = build(&root, &RealFileSystem).expect("build");

    let to_target = inbound_for(&v.backlinks, "target.md");
    let edge = to_target
        .iter()
        .find(|e| e.source.file_name().unwrap() == "host.md")
        .expect("a backlink from the fence record");
    assert_eq!(edge.surface, RefSurface::Frontmatter);
    assert_eq!(edge.slot.as_deref(), Some("ref"));
}

#[test]
fn a_docstring_inside_a_body_fence_record_forms_a_navigational_edge() {
    // A `#:` docstring on a field inside a marked record fence forms a
    // navigational docstring edge, the body-fence twin of a frontmatter docstring.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = fs::canonicalize(dir.path()).expect("canonicalize");
    crate::seed_repo(&root);

    fs::create_dir(root.join("type")).unwrap();
    fs::write(root.join("type/step.type.yaml"), "fields:\n  n?: String\n").unwrap();
    fs::write(root.join("type/host.type.yaml"), "fields:\n  slot: step\n").unwrap();
    fs::write(
        root.join("host.md"),
        "---\ntype: host\nslot:\n---\n\n```yaml [:slot]\ntype: step\nn: x   #: see [[doc]]\n```\n",
    )
    .unwrap();
    fs::write(root.join("doc.md"), "---\ntype: note\n---\n").unwrap();

    let v = build(&root, &RealFileSystem).expect("build");

    let to_doc = inbound_for(&v.backlinks, "doc.md");
    let edge = to_doc
        .iter()
        .find(|e| e.source.file_name().unwrap() == "host.md")
        .expect("a docstring edge from the fence record");
    assert_eq!(edge.surface, RefSurface::Docstring);
    assert_eq!(edge.slot.as_deref(), Some("n"));
}

#[test]
fn a_dangling_docstring_link_warns() {
    // A docstring link to a missing file surfaces the navigational warning, the
    // same advisory stance as a body prose link, on a type-def field.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = fs::canonicalize(dir.path()).expect("canonicalize");
    crate::seed_repo(&root);

    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/task.type.yaml"),
        "fields:\n  gate: String   #: see [[nonexistent-doc]]\n",
    )
    .unwrap();

    let v = build(&root, &RealFileSystem).expect("build");
    let codes: Vec<String> = v
        .diagnostics()
        .map(|d| d.code.as_str().to_string())
        .collect();
    assert!(
        codes.iter().any(|c| c == "navigational-target-not-found"),
        "a dangling type-def docstring link should warn, got {codes:?}"
    );
}

#[test]
fn a_resolving_docstring_link_does_not_warn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = fs::canonicalize(dir.path()).expect("canonicalize");
    crate::seed_repo(&root);

    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/task.type.yaml"),
        "fields:\n  gate: String   #: see [[gate-policy]]\n",
    )
    .unwrap();
    fs::write(root.join("gate-policy.md"), "---\ntype: note\n---\n").unwrap();

    let v = build(&root, &RealFileSystem).expect("build");
    assert!(
        v.diagnostics()
            .all(|d| d.code.as_str() != "navigational-target-not-found"),
        "a resolving docstring link must not warn"
    );
}

#[test]
fn def_ref_value_is_an_edge_to_the_type_def() {
    // A `type<T>*` value points at a type-def by its type-NAME. The design
    // promises a real graph edge from the instance to the def file.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = fs::canonicalize(dir.path()).expect("canonicalize");
    crate::seed_repo(&root);

    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/mcp.tool.type.yaml"),
        "fields:\n  x?: String\n",
    )
    .unwrap();
    fs::write(
        root.join("type/mcp.tool.propose.type.yaml"),
        "extends: mcp.tool\n",
    )
    .unwrap();
    fs::write(
        root.join("type/mode.type.yaml"),
        "fields:\n  propose_tool: type<mcp.tool>*\n",
    )
    .unwrap();
    // The instance names the def by its type-name, not the file stem.
    fs::write(
        root.join("m.md"),
        "---\ntype: mode\npropose_tool: \"[[mcp.tool.propose]]\"\n---\n",
    )
    .unwrap();

    let v = build(&root, &RealFileSystem).expect("build");

    let to_def = inbound_for(&v.backlinks, "mcp.tool.propose.type.yaml");
    assert!(
        has_source_named(to_def, "m.md"),
        "the def file has an inbound edge from the instance, got {to_def:?}"
    );
    let edge = to_def
        .iter()
        .find(|e| e.source.file_name().unwrap() == "m.md")
        .unwrap();
    assert_eq!(edge.surface, RefSurface::Frontmatter);
    assert_eq!(edge.slot.as_deref(), Some("propose_tool"));
}

#[test]
fn adding_and_removing_a_link_updates_both_directions() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Canonicalize so walked paths and resolved paths share one form (macOS
    // tempdirs are symlinks).
    let root = fs::canonicalize(dir.path()).expect("canonicalize");
    crate::seed_repo(&root);

    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  link?: file*\n",
    )
    .unwrap();
    fs::write(root.join("a.md"), "---\ntype: note\nlink: \"[[b]]\"\n---\n").unwrap();
    fs::write(root.join("b.md"), "---\ntype: note\n---\n").unwrap();

    // a -> b through the `link` frontmatter slot.
    let v = build(&root, &RealFileSystem).expect("build 1");
    let to_b = inbound_for(&v.backlinks, "b.md");
    assert!(has_source_named(to_b, "a.md"), "b has inbound from a");
    assert_eq!(to_b[0].slot.as_deref(), Some("link"));
    assert_eq!(to_b[0].surface, RefSurface::Frontmatter);
    assert!(
        !has_source_named(inbound_for(&v.backlinks, "a.md"), "b.md"),
        "a has no inbound yet"
    );

    // Remove the link from a, add one from b -> a.
    fs::write(root.join("a.md"), "---\ntype: note\n---\n").unwrap();
    fs::write(root.join("b.md"), "---\ntype: note\nlink: \"[[a]]\"\n---\n").unwrap();

    let v = build(&root, &RealFileSystem).expect("build 2");
    assert!(
        !has_source_named(inbound_for(&v.backlinks, "b.md"), "a.md"),
        "b's inbound from a is gone after the link was removed"
    );
    let to_a = inbound_for(&v.backlinks, "a.md");
    assert!(
        has_source_named(to_a, "b.md"),
        "a now has inbound from b, got {to_a:?}"
    );
}

#[test]
fn a_wikilink_field_inside_an_inline_record_indexes_an_inbound_edge() {
    // The harness trace shape: a session-log's events are inline records
    // whose reference-shaped fields name the files a tool touched. Those
    // wikilinks must land in the reference graph, so backlinks(notes/foo)
    // lists the sessions that touched it.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::create_dir_all(root.join("operations")).unwrap();
    fs::create_dir(root.join("notes")).unwrap();
    fs::write(
        root.join("type/session-log.type.yaml"),
        "fields:\n  session: String\n  events?: sessionEvent[]\n",
    )
    .unwrap();
    fs::write(
        root.join("type/sessionEvent.type.yaml"),
        "fields:\n  at: String\n  file?: file*\n",
    )
    .unwrap();
    fs::write(
        root.join("notes/foo.md"),
        "---\ntype: sessionEvent\nat: x\n---\n",
    )
    .unwrap();
    fs::write(
        root.join("operations/s-001.yaml"),
        "type: session-log\nsession: \"s-001\"\nevents:\n  - ^: e-write\n    type: sessionEvent\n    at: \"t1\"\n    file: \"[[notes/foo]]\"\n",
    )
    .unwrap();

    let v = build(&root, &RealFileSystem).expect("build");
    let to_foo = inbound_for(&v.backlinks, "foo.md");
    assert!(
        has_source_named(to_foo, "s-001.yaml"),
        "notes/foo.md has an inbound edge from the session log, got {to_foo:?}"
    );
    let edge = to_foo
        .iter()
        .find(|e| e.source.file_name().unwrap() == "s-001.yaml")
        .unwrap();
    assert_eq!(edge.slot.as_deref(), Some("file"));
    assert_eq!(edge.surface, RefSurface::Frontmatter);
    // The edge names the originating event record, so a consumer renders
    // `[[s-001^e-write]]` without re-reading the session.
    assert_eq!(edge.source_block_id.as_deref(), Some("e-write"));
}

#[test]
fn a_commit_referent_forms_no_backlink_and_never_self_links() {
    // `[[::@sha]]` / `[[::repo@sha]]` name a COMMIT, not a file: they resolve to
    // no path, so they never enter the backlink index. The this-repo form in
    // particular must not self-link the source into its own inbound set (the
    // latent hazard: an empty target used to resolve to the source file).
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/span.type.yaml"),
        "fields:\n  commits?: String\n",
    )
    .unwrap();
    // A frontmatter commit-referent (this-repo) plus a body one naming a peer.
    fs::write(
        root.join("s.md"),
        "---\ntype: span\ncommits: \"[[::@a1b2c3d]]\"\n---\nProduced [[::peer@a1b2c3d]].\n",
    )
    .unwrap();

    let v = build(&root, &RealFileSystem).expect("build");

    // No self-link: s.md is not in its own inbound set.
    assert!(
        !has_source_named(inbound_for(&v.backlinks, "s.md"), "s.md"),
        "a commit-referent must not self-link s.md, got {:?}",
        inbound_for(&v.backlinks, "s.md")
    );
    // No inbound edge anywhere carries s.md as a source: the commit-referents
    // resolve to no file, so they form no backlink at all.
    let from_s: Vec<_> = v
        .backlinks
        .iter()
        .flat_map(|(_, edges)| edges.iter())
        .filter(|e| e.source.file_name().map(|n| n == "s.md").unwrap_or(false))
        .collect();
    assert!(
        from_s.is_empty(),
        "commit-referents form no backlink, got {from_s:?}"
    );
}

#[test]
fn a_file_pin_forms_no_backlink_while_a_live_link_does() {
    // A commit-pinned reference `[[note-a::@sha]]` is an inert snapshot: it forms
    // NO inbound backlink on the live file that currently bears the name. An
    // unpinned `[[note-a]]` forms one as usual. This is what stops name-reuse
    // misattribution: the pin never attaches to whatever file holds the name now.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(root.join("type/note.type.yaml"), "fields:\n  ref?: file*\n").unwrap();
    fs::write(
        root.join("note-a.md"),
        "---\ntype: note\n---\nThe target.\n",
    )
    .unwrap();
    // One referrer pins, one links live, both naming the same live file.
    fs::write(
        root.join("pinner.md"),
        "---\ntype: note\nref: \"[[note-a::@a1b2c3d]]\"\n---\n",
    )
    .unwrap();
    fs::write(
        root.join("liver.md"),
        "---\ntype: note\nref: \"[[note-a]]\"\n---\n",
    )
    .unwrap();

    let v = build(&root, &RealFileSystem).expect("build");
    let inbound = inbound_for(&v.backlinks, "note-a.md");

    assert!(
        has_source_named(inbound, "liver.md"),
        "an unpinned reference must form a backlink, got {inbound:?}"
    );
    assert!(
        !has_source_named(inbound, "pinner.md"),
        "a commit-pinned reference must form no backlink, got {inbound:?}"
    );
}
