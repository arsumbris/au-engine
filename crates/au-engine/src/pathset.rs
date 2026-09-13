//! Path-set change support: the work an add, delete, or rename forces beyond a
//! content edit. A path-set change reshapes a repo's reference index and flips
//! the resolution of every edge that named the appearing or disappearing file.
//!
//! This holds the per-repo index rebuild. The edge-flip set, found through the
//! held [`crate::refnames`] index, joins it as the add/delete/rename fast path
//! takes shape.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::ir::OrdMap;
use crate::refnames::{file_name_keys, NameKey};
use crate::repo::{RepoMap, RepoName};

/// The sources whose edge resolution flips when the given paths appear or
/// disappear: every source naming one of them, found through the referenced-name
/// index.
///
/// A changed file is looked up under the keys it would be matched by
/// ([`file_name_keys`]), in its own repo, so a name's appearance or
/// disappearance flips exactly the edges that name it. Covers typed and
/// navigational edges, since existence affects navigational warnings too, and
/// spans repos: the index is repo-qualified on the target side, so a
/// `[[name::repoA]]` edge from repo B is found when a file appears in repo A.
///
/// `changed` may hold files not yet in the catalog (an add) or already gone (a
/// delete); the lookup keys on the path's name and repo, which both forms carry.
/// The result may include the changed paths themselves when one names another;
/// the caller separates those.
pub(crate) fn edge_flip_sources(
    referenced_names: &OrdMap<(RepoName, NameKey), BTreeSet<PathBuf>>,
    repos: &RepoMap,
    changed: impl IntoIterator<Item = PathBuf>,
) -> BTreeSet<PathBuf> {
    let mut sources: BTreeSet<PathBuf> = BTreeSet::new();
    for path in changed {
        let Some(repo) = repos.repo_of(&path) else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(&repo.root) else {
            continue;
        };
        for key in file_name_keys(relative) {
            if let Some(srcs) = referenced_names.get(&(repo.name.clone(), key)) {
                sources.extend(srcs.iter().cloned());
            }
        }
    }
    sources
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::build;
    use au_parser::MemoryFileSystem;
    use std::path::Path;

    #[test]
    fn edge_flip_set_finds_same_repo_and_cross_repo_referrers() {
        // base holds n, referenced from m in base (`[[n]]`) and from t in app
        // (`[[n::base]]`). A change to base's n flips both referrers, the
        // cross-repo one found through the repo-qualified index. A bystander
        // that names something else is left out.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/ws/.arsumbris/repo.yaml", b"name: ws\n".to_vec());
        fs.insert(
            "/ws/.arsumbris/workspace.yaml",
            b"edit:\n  - ws\n  - base\n  - app\n".to_vec(),
        );
        fs.insert("/ws/base/.arsumbris/repo.yaml", b"name: base\n".to_vec());
        fs.insert(
            "/ws/base/type/note.type.yaml",
            b"fields:\n  link?: note*\n".to_vec(),
        );
        fs.insert("/ws/base/n.md", b"---\ntype: note\n---\n".to_vec());
        fs.insert(
            "/ws/base/m.md",
            b"---\ntype: note\n---\nsee [[n]]\n".to_vec(),
        );
        fs.insert(
            "/ws/base/x.md",
            b"---\ntype: note\n---\nsee [[other]]\n".to_vec(),
        );
        fs.insert(
            "/ws/app/.arsumbris/repo.yaml",
            b"name: app\ndeps:\n  - name: base\n".to_vec(),
        );
        fs.insert(
            "/ws/app/type/note.type.yaml",
            b"fields:\n  link?: note*\n".to_vec(),
        );
        fs.insert(
            "/ws/app/t.md",
            b"---\ntype: note\n---\nsee [[n::base]]\n".to_vec(),
        );
        let kb = build(Path::new("/ws"), &fs).unwrap();

        let flipped = edge_flip_sources(
            &kb.referenced_names,
            &kb.repos,
            [PathBuf::from("/ws/base/n.md")],
        );
        assert!(
            flipped.contains(Path::new("/ws/base/m.md")),
            "same-repo referrer"
        );
        assert!(
            flipped.contains(Path::new("/ws/app/t.md")),
            "cross-repo referrer"
        );
        assert!(
            !flipped.contains(Path::new("/ws/base/x.md")),
            "a source naming something else is not flipped"
        );
    }
}
