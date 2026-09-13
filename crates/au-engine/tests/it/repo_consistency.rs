//! Cross-repo consistency warnings: unmounted peers and dependency-identity
//! conflicts.
//!
//! Each is advisory, resolved per-repo over the declared registry.

use std::fs;
use std::path::Path;

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

#[test]
fn peer_unmounted_when_no_path_on_this_machine() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    // A declared peer, but no location file for this machine.
    write(
        &root,
        ".arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(&root, "doc.md", "x");

    let kb = build(&root, &RealFileSystem).expect("build");
    assert!(
        codes(&kb).iter().any(|c| c == "peer-unmounted"),
        "got {:?}",
        codes(&kb)
    );
}

#[test]
fn dependency_identity_conflict_when_dep_remote_disagrees_with_owner() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["app", "base"]);
    // `app` declares dep `base` asserting a remote that disagrees with `base`'s
    // own declared remote, the canonical owner.
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n    remote: git@x:me/WRONG.git\n",
    );
    write(&root, "app/note.md", "x");
    write(
        &root,
        "base/.arsumbris/repo.yaml",
        "name: base\nremote: git@x:me/base.git\n",
    );
    write(&root, "base/note.md", "y");

    let kb = build(&root, &RealFileSystem).expect("build");
    assert!(
        codes(&kb)
            .iter()
            .any(|c| c == "dependency-identity-conflict"),
        "got {:?}",
        codes(&kb)
    );
}

#[test]
fn no_conflict_when_dep_remote_matches_or_is_absent() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["app", "base", "lib"]);
    // a matching remote assertion and a name-only dep are both fine.
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n    remote: git@x:me/base.git\n  - name: lib\n",
    );
    write(&root, "app/note.md", "x");
    write(
        &root,
        "base/.arsumbris/repo.yaml",
        "name: base\nremote: git@x:me/base.git\n",
    );
    write(&root, "base/note.md", "y");
    write(
        &root,
        "lib/.arsumbris/repo.yaml",
        "name: lib\nremote: git@x:me/lib.git\n",
    );
    write(&root, "lib/note.md", "z");

    let kb = build(&root, &RealFileSystem).expect("build");
    assert!(
        !codes(&kb)
            .iter()
            .any(|c| c == "dependency-identity-conflict"),
        "got {:?}",
        codes(&kb)
    );
}
