//! Workspace assembly: the engine pointed at a folder-repo directory composes
//! one knowledge base from its `.arsumbris/workspace.yaml`'s members plus their transitive
//! `deps` closure, each member walked from its own root.
//!
//! The members here are co-present subdirectories of the entry folder-repo,
//! resolved by name. Assembly makes them present, so the per-repo graphs, the
//! cross-repo site set, and `::repo` resolution compose over them unchanged.

use std::fs;
use std::path::Path;

use au_core::TypeName;
use au_engine::build;
use au_parser::RealFileSystem;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn codes(kb: &au_engine::KnowledgeBase) -> Vec<String> {
    kb.diagnostics()
        .map(|d| d.code.as_str().to_string())
        .collect()
}

/// An assembled workspace: the entry `ws/` is a content-free folder-repo whose
/// `.arsumbris/workspace.yaml` composes the members `base` and `app`, co-present
/// subdirectories of `ws/`. `base` owns `note`; `app` owns its own `note` (a
/// distinct same-named identity) and `task`.
fn assembled_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let tmp = fs::canonicalize(dir.path()).unwrap();

    write(&tmp, "ws/.arsumbris/repo.yaml", "name: ws\n");
    write(
        &tmp,
        "ws/.arsumbris/workspace.yaml",
        "edit:\n  - ws\n  - base\n  - app\n",
    );

    // base, owns note.
    write(&tmp, "ws/base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &tmp,
        "ws/base/type/note.type.yaml",
        "fields:\n  title: String\n",
    );
    write(&tmp, "ws/base/n.md", "---\ntype: note\ntitle: hi\n---\n");

    // app, owns its own note and task. It declares base as a co-present
    // dependency.
    write(
        &tmp,
        "ws/app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(
        &tmp,
        "ws/app/type/note.type.yaml",
        "fields:\n  title: String\n",
    );
    write(
        &tmp,
        "ws/app/type/task.type.yaml",
        "fields:\n  done: Boolean\n",
    );
    // A cross-repo body reference into the `base` member.
    write(
        &tmp,
        "ws/app/t.md",
        "---\ntype: task\ndone: false\n---\nsee [[n::base]]\n",
    );

    let ws = tmp.join("ws");
    (dir, ws)
}

#[test]
fn assembles_one_kb_from_co_present_members() {
    let (_dir, ws) = assembled_workspace();
    let kb = build(&ws, &RealFileSystem).expect("build");

    // Both members composed into one knowledge base: each repo's graph holds its defs.
    let base = kb.graphs.of(&au_engine::RepoName("base".into()));
    let app = kb.graphs.of(&au_engine::RepoName("app".into()));
    assert!(
        base.get(&TypeName("note".into())).is_some(),
        "base's note in the composed knowledge base"
    );
    assert!(
        app.get(&TypeName("note".into())).is_some(),
        "app's own note in the composed knowledge base"
    );
    assert!(
        app.get(&TypeName("task".into())).is_some(),
        "app's task in the composed knowledge base"
    );

    // The entry workspace is held with the entry repo plus both members in scope,
    // name-sorted.
    assert_eq!(kb.workspaces.len(), 1, "one assembled workspace");
    let members: Vec<&str> = kb.workspaces[0]
        .members
        .iter()
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(members, vec!["app", "base", "ws"]);
}

#[test]
fn instances_in_every_member_validate() {
    let (_dir, ws) = assembled_workspace();
    let kb = build(&ws, &RealFileSystem).expect("build");

    // Each member's instance resolved against its own repo's vocabulary.
    assert!(
        kb.instances.contains_key(&ws.join("base/n.md")),
        "base's note instance resolved"
    );
    assert!(
        kb.instances.contains_key(&ws.join("app/t.md")),
        "app's task instance resolved"
    );
    // No unknown-type-claim: every claim resolved within its member.
    assert!(
        !codes(&kb).iter().any(|c| c == "unknown-type-claim"),
        "every claim resolved, got {:?}",
        codes(&kb)
    );
}

// Retired with the folder-repo entry flip: `a_directory_with_several_manifests_is_
// tree_mode_not_ambiguous` and `a_manifest_file_entry_assembles_only_its_primaries`
// tested the removed `*.au-workspace.yaml` FILE entry and bare-directory tree mode.
// The entry is now always a folder-repo directory (see the folder-repo tests below).

#[test]
fn a_content_less_member_is_discovered_by_its_walker_marker() {
    // A declared member holding ONLY `.arsumbris/repo.yaml` (no content) is
    // discovered by its walker-surfaced marker. Before marker discovery it was
    // invisible: the walk skips `.arsumbris/`, so a content-less repo had no
    // walked file to anchor the ancestor-scan discovery. Its discovery is
    // observable via the folder-name drift, which also proves discovery is
    // folder-name-independent (the marker, not the folder, is the indicator).
    let dir = tempfile::tempdir().unwrap();
    let tmp = fs::canonicalize(dir.path()).unwrap();
    // A folder-repo entry composing `main` and the content-less `declared` member.
    write(&tmp, ".arsumbris/repo.yaml", "name: home\n");
    write(
        &tmp,
        ".arsumbris/workspace.yaml",
        "edit:\n  - home\n  - main\n  - declared\n",
    );
    // A member with content, so the tree has files to walk.
    write(&tmp, "main/.arsumbris/repo.yaml", "name: main\n");
    write(&tmp, "main/type/x.type.yaml", "fields:\n  a: String\n");
    // A content-less member in a folder whose name differs from its declared name.
    write(
        &tmp,
        "wrong-folder/.arsumbris/repo.yaml",
        "name: declared\n",
    );

    let kb = build(&tmp, &RealFileSystem).expect("build");
    assert!(
        codes(&kb).iter().any(|c| c == "repo-folder-name-mismatch"),
        "the content-less member is discovered by its marker, so its folder/name drift fires: {:?}",
        codes(&kb)
    );
}

#[test]
fn a_missing_edit_member_is_edit_member_unmounted_and_the_workspace_opens_degraded() {
    // A manifest names two edit members: `base` is co-present, `ghost` resolves to
    // nothing. `ghost` is an editable root the user named, so its unmounted
    // outcome is `edit-member-unmounted`, and the workspace still opens degraded
    // over its present edit member `base`.
    let dir = tempfile::tempdir().unwrap();
    let tmp = fs::canonicalize(dir.path()).unwrap();
    write(&tmp, "ws/.arsumbris/repo.yaml", "name: ws\n");
    write(
        &tmp,
        "ws/.arsumbris/workspace.yaml",
        "edit:\n  - ws\n  - base\n  - ghost\n",
    );
    write(&tmp, "ws/base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &tmp,
        "ws/base/type/note.type.yaml",
        "fields:\n  title: String\n",
    );
    write(&tmp, "ws/base/n.md", "---\ntype: note\ntitle: hi\n---\n");

    // `ghost` is an edit member that resolves to nothing.
    let kb = build(&tmp.join("ws"), &RealFileSystem).expect("build");

    let cs = codes(&kb);
    assert!(
        cs.iter().any(|c| c == "edit-member-unmounted"),
        "the unresolved edit member reads as edit-member-unmounted, got {cs:?}"
    );
    // The workspace still opens over the present primary.
    assert!(
        kb.graphs
            .of(&au_engine::RepoName("base".into()))
            .get(&TypeName("note".into()))
            .is_some(),
        "the present primary still composes"
    );
}

#[test]
fn a_name_in_both_edit_and_discover_is_a_role_conflict() {
    // `base` appears in both `edit:` and `discover:`. A member has one role per
    // workspace, so this is `workspace-member-role-conflict`. The editable role
    // wins meanwhile, so the workspace still opens over `base`.
    let dir = tempfile::tempdir().unwrap();
    let tmp = fs::canonicalize(dir.path()).unwrap();
    write(&tmp, "ws/.arsumbris/repo.yaml", "name: ws\n");
    write(
        &tmp,
        "ws/.arsumbris/workspace.yaml",
        "edit:\n  - ws\n  - base\ndiscover:\n  - base\n",
    );
    write(&tmp, "ws/base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &tmp,
        "ws/base/type/note.type.yaml",
        "fields:\n  title: String\n",
    );

    let kb = build(&tmp.join("ws"), &RealFileSystem).expect("build");

    let cs = codes(&kb);
    assert!(
        cs.iter().any(|c| c == "workspace-member-role-conflict"),
        "a name in both edit and discover conflicts, got {cs:?}"
    );
    // The editable role wins, so `base` still composes.
    assert!(
        kb.graphs
            .of(&au_engine::RepoName("base".into()))
            .get(&TypeName("note".into()))
            .is_some(),
        "the editable role wins and the member composes"
    );
}

#[test]
fn a_folder_repo_directory_assembles_from_its_workspace_yaml() {
    // The entry is a DIRECTORY that is itself a repo (`home`) carrying a
    // `.arsumbris/workspace.yaml` composing itself plus a nested member `sub`.
    // Pointing at the folder assembles from that composition, not a tree scan.
    let dir = tempfile::tempdir().unwrap();
    let tmp = fs::canonicalize(dir.path()).unwrap();
    write(&tmp, ".arsumbris/repo.yaml", "name: home\n");
    write(
        &tmp,
        ".arsumbris/workspace.yaml",
        "edit:\n  - home\n  - sub\n",
    );
    write(&tmp, "type/note.type.yaml", "fields:\n  title: String\n");
    write(&tmp, "sub/.arsumbris/repo.yaml", "name: sub\n");
    write(
        &tmp,
        "sub/type/task.type.yaml",
        "fields:\n  done: Boolean\n",
    );

    // Point at the DIRECTORY (folder-repo entry), not a `*.au-workspace.yaml`.
    let kb = build(&tmp, &RealFileSystem).expect("build");

    assert!(
        kb.graphs
            .of(&au_engine::RepoName("home".into()))
            .get(&TypeName("note".into()))
            .is_some(),
        "the entry repo composes"
    );
    assert!(
        kb.graphs
            .of(&au_engine::RepoName("sub".into()))
            .get(&TypeName("task".into()))
            .is_some(),
        "the edit member composes"
    );
    let cs = codes(&kb);
    assert!(
        !cs.iter().any(|c| c == "workspace-omits-containing-repo"),
        "the containing repo is listed in edit, got {cs:?}"
    );
}

#[test]
fn a_workspace_yaml_omitting_its_containing_repo_errors_but_still_mounts_it() {
    // The `.arsumbris/workspace.yaml`'s `edit` omits its containing repo `home`.
    // That is `workspace-omits-containing-repo` (self-completeness), but the entry
    // repo is ALWAYS a member (role Entry), so it still composes.
    let dir = tempfile::tempdir().unwrap();
    let tmp = fs::canonicalize(dir.path()).unwrap();
    write(&tmp, ".arsumbris/repo.yaml", "name: home\n");
    write(&tmp, ".arsumbris/workspace.yaml", "edit:\n  - sub\n");
    write(&tmp, "type/note.type.yaml", "fields:\n  title: String\n");
    write(&tmp, "sub/.arsumbris/repo.yaml", "name: sub\n");

    let kb = build(&tmp, &RealFileSystem).expect("build");

    let cs = codes(&kb);
    assert!(
        cs.iter().any(|c| c == "workspace-omits-containing-repo"),
        "omitting the containing repo errors, got {cs:?}"
    );
    assert!(
        kb.graphs
            .of(&au_engine::RepoName("home".into()))
            .get(&TypeName("note".into()))
            .is_some(),
        "the entry repo mounts regardless of the omission (role Entry)"
    );
}

#[test]
fn an_undeclared_nested_repo_is_skipped_with_a_warning() {
    // A folder-repo `home` with a nested repo `sub` NOT declared in the
    // workspace.yaml. The nested repo is skipped (its subtree contributes
    // nothing) and reported `undeclared-nested-repo`, never silently absorbed.
    let dir = tempfile::tempdir().unwrap();
    let tmp = fs::canonicalize(dir.path()).unwrap();
    write(&tmp, ".arsumbris/repo.yaml", "name: home\n");
    write(&tmp, ".arsumbris/workspace.yaml", "edit:\n  - home\n");
    write(&tmp, "type/note.type.yaml", "fields:\n  title: String\n");
    write(&tmp, "sub/.arsumbris/repo.yaml", "name: sub\n");
    write(
        &tmp,
        "sub/type/task.type.yaml",
        "fields:\n  done: Boolean\n",
    );

    let kb = build(&tmp, &RealFileSystem).expect("build");

    let cs = codes(&kb);
    assert!(
        cs.iter().any(|c| c == "undeclared-nested-repo"),
        "the undeclared nested repo is skipped with a warning, got {cs:?}"
    );
    assert!(
        kb.graphs
            .of(&au_engine::RepoName("home".into()))
            .get(&TypeName("note".into()))
            .is_some(),
        "the entry repo still composes"
    );
    assert!(
        kb.graphs
            .of(&au_engine::RepoName("sub".into()))
            .get(&TypeName("task".into()))
            .is_none(),
        "the skipped nested repo contributes no types"
    );
}

#[test]
fn a_declared_nested_repo_mounts_and_does_not_warn() {
    // The same nested repo, but declared in `edit`, mounts as its own member and
    // raises no `undeclared-nested-repo`.
    let dir = tempfile::tempdir().unwrap();
    let tmp = fs::canonicalize(dir.path()).unwrap();
    write(&tmp, ".arsumbris/repo.yaml", "name: home\n");
    write(
        &tmp,
        ".arsumbris/workspace.yaml",
        "edit:\n  - home\n  - sub\n",
    );
    write(&tmp, "type/note.type.yaml", "fields:\n  title: String\n");
    write(&tmp, "sub/.arsumbris/repo.yaml", "name: sub\n");
    write(
        &tmp,
        "sub/type/task.type.yaml",
        "fields:\n  done: Boolean\n",
    );

    let kb = build(&tmp, &RealFileSystem).expect("build");

    let cs = codes(&kb);
    assert!(
        !cs.iter().any(|c| c == "undeclared-nested-repo"),
        "a declared nested repo does not warn, got {cs:?}"
    );
    assert!(
        kb.graphs
            .of(&au_engine::RepoName("sub".into()))
            .get(&TypeName("task".into()))
            .is_some(),
        "the declared nested repo mounts and composes"
    );
}

#[test]
fn a_co_present_repo_reference_resolves_into_a_mounted_member() {
    let (_dir, ws) = assembled_workspace();
    let kb = build(&ws, &RealFileSystem).expect("build");

    // app/t.md says `see [[n::base]]`. base is a mounted member, so the `::repo`
    // link resolves into it. Before assembly it was reference-repo-unavailable.
    let cs = codes(&kb);
    assert!(
        !cs.iter().any(|c| c == "reference-repo-unavailable"),
        "the co-present peer is mounted, got {:?}",
        cs
    );
    assert!(
        !cs.iter().any(|c| c == "reference-repo-unknown"),
        "base is a declared peer, got {:?}",
        cs
    );
    // The reference resolved to base's note, no dangling target.
    assert!(
        !cs.iter().any(|c| c == "reference-target-missing"),
        "the cross-repo target resolved, got {:?}",
        cs
    );
}

#[test]
fn an_unmounted_member_drops_out_with_an_advisory() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = fs::canonicalize(dir.path()).unwrap();

    // The workspace declares base and finance as edit members; only base is
    // co-present. finance resolves nowhere, a legitimate state. Both are editable
    // members, so finance's unmounted outcome reads as `edit-member-unmounted`.
    write(&tmp, "ws/.arsumbris/repo.yaml", "name: ws\n");
    write(
        &tmp,
        "ws/.arsumbris/workspace.yaml",
        "edit:\n  - ws\n  - base\n  - finance\n",
    );
    write(&tmp, "ws/base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &tmp,
        "ws/base/type/note.type.yaml",
        "fields:\n  title: String\n",
    );
    write(&tmp, "ws/base/n.md", "---\ntype: note\ntitle: hi\n---\n");

    let ws = tmp.join("ws");
    let kb = build(&ws, &RealFileSystem).expect("build");

    // finance dropped out with the advisory; base still assembled. An edit
    // member's unmounted outcome reads as edit-member-unmounted.
    assert!(
        codes(&kb).iter().any(|c| c == "edit-member-unmounted"),
        "finance is an unmounted edit member, got {:?}",
        codes(&kb)
    );
    assert!(
        kb.graphs
            .of(&au_engine::RepoName("base".into()))
            .get(&TypeName("note".into()))
            .is_some(),
        "the workspace still assembles its present member"
    );
}

#[test]
fn a_discover_member_that_is_also_a_dep_is_a_redundant_hint() {
    // `shared` is a declared dep of `app` AND listed in `discover`. During
    // assembly it resolves to the higher role (dep subsumes discover), so the
    // `discover` listing is redundant: discover-member-is-a-dependency (hint),
    // never a role conflict (that is edit + discover).
    let dir = tempfile::tempdir().unwrap();
    let tmp = fs::canonicalize(dir.path()).unwrap();
    write(&tmp, "ws/.arsumbris/repo.yaml", "name: ws\n");
    write(
        &tmp,
        "ws/.arsumbris/workspace.yaml",
        "edit:\n  - ws\n  - app\ndiscover:\n  - shared\n",
    );
    write(
        &tmp,
        "ws/app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: shared\n",
    );
    write(&tmp, "ws/app/a.md", "x");
    write(&tmp, "ws/shared/.arsumbris/repo.yaml", "name: shared\n");
    write(&tmp, "ws/shared/s.md", "x");

    let kb = build(&tmp.join("ws"), &RealFileSystem).expect("build");
    let cs = codes(&kb);
    assert!(
        cs.iter().any(|c| c == "discover-member-is-a-dependency"),
        "a discover that is also a dep is a redundant hint, got {cs:?}"
    );
    assert!(
        !cs.iter().any(|c| c == "workspace-member-role-conflict"),
        "a redundant discover is not a role conflict, got {cs:?}"
    );
}

#[test]
fn a_properly_mounted_member_raises_no_membership_advisory() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = fs::canonicalize(dir.path()).unwrap();
    write(&tmp, "ws/.arsumbris/repo.yaml", "name: ws\n");
    write(
        &tmp,
        "ws/.arsumbris/workspace.yaml",
        "edit:\n  - ws\n  - alpha\n",
    );
    write(&tmp, "ws/alpha/.arsumbris/repo.yaml", "name: alpha\n");
    write(&tmp, "ws/alpha/doc.md", "x");

    let kb = build(&tmp.join("ws"), &RealFileSystem).expect("build");
    let cs = codes(&kb);
    assert!(
        !cs.iter().any(|c| c == "edit-member-unmounted"),
        "edit-member-unmounted should not fire for a mounted member, got {cs:?}"
    );
}

#[test]
fn members_read_reports_co_present_members() {
    let (_dir, ws) = assembled_workspace();
    let kb = build(&ws, &RealFileSystem).expect("build");

    // Both members live under `ws/`, so neither is scattered.
    let view = au_engine::wire::introspect_members(&kb, &ws, None);
    let by_name: std::collections::BTreeMap<&str, &au_engine::wire::MemberView> =
        view.members.iter().map(|m| (m.repo.as_str(), m)).collect();
    let base = by_name.get("base").expect("base member");
    let app = by_name.get("app").expect("app member");
    assert!(!base.scattered, "base is co-present under ws");
    assert!(!app.scattered, "app is co-present under ws");
    // base and app are both `edit` members, so both editable (role-derived).
    assert!(base.editable, "an edit member is editable");
    assert!(app.editable, "an edit member is editable");
    assert!(
        std::path::Path::new(&base.root).is_absolute(),
        "the member root is absolute"
    );

    // resolve_member maps a member's file back to it.
    let in_base = ws.join("base/n.md");
    let owner = au_engine::wire::resolve_member(&kb, &in_base, None).expect("a file under base");
    assert_eq!(owner.repo, "base");
}

#[test]
fn members_and_resolve_member_report_role_derived_editable() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = fs::canonicalize(dir.path()).unwrap();
    write(&tmp, "ws/.arsumbris/repo.yaml", "name: ws\n");
    write(
        &tmp,
        "ws/.arsumbris/workspace.yaml",
        "edit:\n  - ws\n  - app\n",
    );
    write(
        &tmp,
        "ws/app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: lib\n",
    );
    write(&tmp, "ws/app/a.md", "x");
    write(&tmp, "ws/lib/.arsumbris/repo.yaml", "name: lib\n");
    write(&tmp, "ws/lib/l.md", "x");

    let ws = tmp.join("ws");
    let kb = build(&ws, &RealFileSystem).expect("build");

    // No cache root, so every co-present member is `local`.
    let view = au_engine::wire::introspect_members(&kb, &ws, None);
    let by_name: std::collections::BTreeMap<&str, &au_engine::wire::MemberView> =
        view.members.iter().map(|m| (m.repo.as_str(), m)).collect();
    let app = by_name.get("app").expect("app member");
    let lib = by_name.get("lib").expect("lib member");
    // app is an `edit` member, lib is reached only through app's deps.
    assert_eq!(app.role, "edit", "app is an edit member");
    assert_eq!(lib.role, "dep", "lib is a computed transitive dep");
    // The two axes are orthogonal. `lib` is a co-present LOCAL tree (`local:
    // true`, watched) but a `dep` role, so consumed, not an authoring surface
    // (`editable: false`). This is the decoupling of decision 2607161333: `local`
    // carries served-locally, `editable` carries the role.
    assert!(
        app.editable && app.local,
        "an edit member is editable and local"
    );
    assert!(
        !lib.editable && lib.local,
        "a co-present dep is consumed (not editable) yet local"
    );

    // resolve_member carries the same axes, the cage's write-scoping basis
    // (`editable && local`).
    let owner =
        au_engine::wire::resolve_member(&kb, &ws.join("lib/l.md"), None).expect("a file under lib");
    assert_eq!(owner.role, "dep", "the owning member is a dep");
    assert!(
        !owner.editable && owner.local,
        "the owning co-present dep is consumed yet local"
    );
}
