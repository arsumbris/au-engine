//! The referenced-name index: which sources name each target identity.
//!
//! Where [`crate::backlinks`] inverts the RESOLVED edges by target file, this
//! inverts the NAMES every edge mentions, resolved or dangling, typed or
//! navigational, by the identity a file add, delete, or rename changes. It is
//! the index a path-set change walks to its edge-flip set: the sources whose
//! resolution turns over when a basename appears or disappears.
//!
//! The keys mirror au-references resolution ([`au_references::RepoIndex`]): a
//! bare target matches a file by case-insensitive basename or stem; a
//! `/`-bearing target matches by exact repo-relative path, or by parent plus
//! stem when extensionless. A file is found under every key a target could
//! match it by, so a name's appearance flips exactly the edges that name it.
//!
//! Cross-repo aware: the target side is repo-qualified. A `[[name::repoA]]`
//! edge from repo B keys into repoA, so an add in A flips B's edge. An
//! unqualified edge keys into the source's own repo, resolution is repo-local.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use au_core::{parse_block_record, InstanceValue, NavLink};
use au_parser::{scan_body, BodyEvent};
use au_references::{extract_field_marker, parse_wikilink_inner};

use crate::ir::FileEntry;
use crate::parse::FileParse;
use crate::repo::{RepoMap, RepoName};

/// A normalized wikilink-target identity, the form a file is matched against.
///
/// Mirrors au-references resolution. A file contributes the keys it can be
/// reached by; an edge contributes the keys its target string can match.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum NameKey {
    /// A bare target's lowercased token. A file contributes both its lowercased
    /// basename and its lowercased stem, so a bare edge matches either.
    Bare(String),
    /// A `/`-bearing target's exact repo-relative path.
    Relpath(PathBuf),
    /// A `/`-bearing extensionless target's parent directory plus final stem.
    DirStem(PathBuf, String),
}

/// The keys a wikilink target string can match a file by.
fn target_keys(target: &str) -> Vec<NameKey> {
    if target.is_empty() {
        return Vec::new(); // a local `[[^block]]` self-reference names no file
    }
    if target.contains('/') {
        let rel = PathBuf::from(target);
        let mut keys = vec![NameKey::Relpath(rel.clone())];
        if rel.extension().is_none() {
            if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
                keys.push(NameKey::DirStem(
                    parent.to_path_buf(),
                    name.to_string_lossy().into_owned(),
                ));
            }
        }
        keys
    } else {
        vec![NameKey::Bare(target.to_ascii_lowercase())]
    }
}

/// The keys a file is found under, the identities its add or delete flips.
///
/// `relative` is the file's path relative to its repo root, the form
/// `/`-bearing targets resolve against.
pub fn file_name_keys(relative: &Path) -> Vec<NameKey> {
    let mut keys = Vec::new();
    if let Some(basename) = relative.file_name().and_then(|s| s.to_str()) {
        keys.push(NameKey::Bare(basename.to_ascii_lowercase()));
    }
    if let Some(stem) = relative.file_stem().and_then(|s| s.to_str()) {
        keys.push(NameKey::Bare(stem.to_ascii_lowercase()));
    }
    keys.push(NameKey::Relpath(relative.to_path_buf()));
    if let (Some(parent), Some(stem)) = (
        relative.parent(),
        relative.file_stem().and_then(|s| s.to_str()),
    ) {
        keys.push(NameKey::DirStem(parent.to_path_buf(), stem.to_string()));
    }
    keys
}

/// The `(target-repo, name-key)` pairs one source's links reference.
///
/// The target repo is the link's `::repo` qualifier when present, else the
/// source's own repo, mirroring repo-local resolution. No resolution is run:
/// the index names targets whether or not a file exists for them, since an
/// add is exactly the case where one does not yet.
pub fn source_name_keys(
    parse: &FileParse,
    source_repo: &RepoName,
) -> BTreeSet<(RepoName, NameKey)> {
    let mut out: BTreeSet<(RepoName, NameKey)> = BTreeSet::new();

    // Docstring links, on a type-def or an instance. The name-index twin of the
    // backlink walk's docstring pass, so an add / delete of a documented target
    // flips the documenting source incrementally. A type-def has no other edges,
    // so this is the whole of its contribution.
    let doc_links: &[au_core::DocstringLink] = match parse {
        FileParse::TypeDef { doc_links, .. } | FileParse::Instance { doc_links, .. } => {
            doc_links.as_slice()
        }
        _ => &[],
    };
    for dl in doc_links {
        if let Ok(w) = parse_wikilink_inner(&dl.link.raw) {
            add_target(w.repo.as_deref(), &w.target, source_repo, &mut out);
        }
    }

    let (fields, body, is_markdown) = match parse {
        FileParse::Instance {
            instance: Some(inst),
            body,
            is_markdown,
            ..
        } => (&inst.fields, body, *is_markdown),
        FileParse::Note { fields, body, .. } => (fields, body, true),
        _ => return out,
    };

    for field in fields {
        collect_value_targets(&field.value, &field.nav_links, source_repo, &mut out);
    }
    if is_markdown {
        for ev in scan_body(body) {
            match ev {
                BodyEvent::Wikilink { raw, .. } => {
                    if let Ok(w) = parse_wikilink_inner(raw) {
                        add_target(w.repo.as_deref(), &w.target, source_repo, &mut out);
                    }
                }
                // A marked record fence: its field-value references (now real
                // backlink edges the rename path rewrites) and its own `#:`
                // docstrings must flip too, the same class as a frontmatter
                // record. Name keys ignore spans, so the base offset is
                // irrelevant and the source path is unused.
                BodyEvent::FencedBlock {
                    info,
                    body: fence_body,
                    ..
                } => {
                    if extract_field_marker(info).is_none() {
                        continue;
                    }
                    if let Some((inline, fence_doc_links)) =
                        parse_block_record(Path::new(""), fence_body, 0)
                    {
                        collect_value_targets(
                            &InstanceValue::Mapping(inline),
                            &[],
                            source_repo,
                            &mut out,
                        );
                        for dl in fence_doc_links {
                            if let Ok(w) = parse_wikilink_inner(&dl.link.raw) {
                                add_target(w.repo.as_deref(), &w.target, source_repo, &mut out);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Recurse a field value, mirroring the backlink walk, collecting every
/// navigational wikilink's target into a name-key.
fn collect_value_targets(
    value: &InstanceValue,
    nav_links: &[NavLink],
    source_repo: &RepoName,
    out: &mut BTreeSet<(RepoName, NameKey)>,
) {
    match value {
        InstanceValue::String(_) => {
            for nl in nav_links {
                if let Ok(w) = parse_wikilink_inner(&nl.raw) {
                    add_target(w.repo.as_deref(), &w.target, source_repo, out);
                }
            }
        }
        InstanceValue::Sequence(elems) => {
            for e in elems {
                collect_value_targets(&e.value, &e.nav_links, source_repo, out);
            }
        }
        InstanceValue::Mapping(inline) => {
            for f in &inline.fields {
                collect_value_targets(&f.value, &f.nav_links, source_repo, out);
            }
        }
        _ => {}
    }
}

/// Add a link's keys, qualified by its `::repo` or the source's repo.
fn add_target(
    repo: Option<&str>,
    target: &str,
    source_repo: &RepoName,
    out: &mut BTreeSet<(RepoName, NameKey)>,
) {
    let target_repo = match repo {
        Some(r) => RepoName(r.to_string()),
        None => source_repo.clone(),
    };
    for key in target_keys(target) {
        out.insert((target_repo.clone(), key));
    }
}

/// Invert every source's referenced names into `(repo, key)` to its sources.
pub fn build_index(
    catalog: &crate::ir::OrdMap<PathBuf, FileEntry>,
    repos: &RepoMap,
) -> BTreeMap<(RepoName, NameKey), BTreeSet<PathBuf>> {
    let mut index: BTreeMap<(RepoName, NameKey), BTreeSet<PathBuf>> = BTreeMap::new();
    for (path, entry) in catalog {
        let source_repo = source_repo(repos, path);
        for key in source_name_keys(entry.parse.as_ref(), &source_repo) {
            index.entry(key).or_default().insert(path.clone());
        }
    }
    index
}

/// The repo a source belongs to, falling back to the root repo so a key always
/// has a repo, matching [`crate::build`]'s `repo_of`. The repo a source's
/// unqualified links resolve within, and the one the fast path keys its delta
/// by.
pub fn source_repo(repos: &RepoMap, path: &Path) -> RepoName {
    repos
        .repo_of(path)
        .map(|r| r.name.clone())
        .or_else(|| repos.root().map(|r| r.name.clone()))
        .unwrap_or_else(|| RepoName(String::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::build;
    use au_parser::MemoryFileSystem;

    #[test]
    fn index_keys_by_basename_stem_relpath_and_repo_qualifier() {
        // a names a bare target, a `/`-bearing relpath, and a cross-repo
        // `::other` target. The index must key each in the form a file add
        // would match, basename/stem in-repo, relpath plus dir-stem, and the
        // cross-repo target qualified into the named repo.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  link?: note*\n".to_vec(),
        );
        fs.insert(
            "/v/a.md",
            b"---\ntype: note\n---\nsee [[b]] and [[sub/c]] and [[t::other]]\n".to_vec(),
        );
        fs.insert("/v/b.md", b"---\ntype: note\n---\n".to_vec());
        let kb = build(Path::new("/v"), &fs).unwrap();

        let idx = &kb.referenced_names;
        let v = RepoName("v".into());
        let other = RepoName("other".into());
        let a = PathBuf::from("/v/a.md");
        let names = |repo: &RepoName, key: NameKey| {
            idx.get(&(repo.clone(), key))
                .is_some_and(|sources| sources.contains(&a))
        };

        // Bare `[[b]]` keys the lowercased token in a's own repo.
        assert!(names(&v, NameKey::Bare("b".into())), "bare target");
        // `/`-bearing `[[sub/c]]` keys the exact relpath and, extensionless, the
        // parent-plus-stem.
        assert!(
            names(&v, NameKey::Relpath(PathBuf::from("sub/c"))),
            "relpath"
        );
        assert!(
            names(&v, NameKey::DirStem(PathBuf::from("sub"), "c".into())),
            "dir-stem"
        );
        // `[[t::other]]` keys into the named repo, the target side is qualified.
        assert!(
            names(&other, NameKey::Bare("t".into())),
            "cross-repo qualifier"
        );
        // A file's own basename is found under both its basename and its stem.
        let keys = file_name_keys(Path::new("note.md"));
        assert!(keys.contains(&NameKey::Bare("note.md".into())));
        assert!(keys.contains(&NameKey::Bare("note".into())));
    }

    #[test]
    fn docstring_link_targets_are_indexed_so_incremental_flips() {
        // A `#:` docstring wikilink must register in the referenced-name index,
        // or adding / removing its target does not flip the source
        // incrementally, and incremental diverges from a full build. Covers the
        // type-def head-and-field surface and an instance frontmatter docstring.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/task.type.yaml",
            b"#: see [[headpolicy]]\nfields:\n  gate: String   #: see [[fieldpolicy]]\n".to_vec(),
        );
        fs.insert(
            "/v/a.md",
            b"---\n#: see [[instpolicy]]\ntype: task\ngate: ok\n---\n".to_vec(),
        );
        let kb = build(Path::new("/v"), &fs).unwrap();

        let v = RepoName("v".into());
        let td = PathBuf::from("/v/type/task.type.yaml");
        let inst = PathBuf::from("/v/a.md");
        let names = |key: NameKey, src: &PathBuf| {
            kb.referenced_names
                .get(&(v.clone(), key))
                .is_some_and(|sources| sources.contains(src))
        };

        assert!(
            names(NameKey::Bare("headpolicy".into()), &td),
            "a type-def head docstring link must be indexed"
        );
        assert!(
            names(NameKey::Bare("fieldpolicy".into()), &td),
            "a type-def field docstring link must be indexed"
        );
        assert!(
            names(NameKey::Bare("instpolicy".into()), &inst),
            "an instance head docstring link must be indexed"
        );
    }

    #[test]
    fn fence_record_reference_targets_are_indexed() {
        // The rename-hole fix made a fence-record field reference a real backlink
        // edge; it must also register in the referenced-name index so its
        // dangling state flips incrementally, the twin of the docstring gap.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/step.type.yaml",
            b"fields:\n  ref: file*\n".to_vec(),
        );
        fs.insert(
            "/v/type/host.type.yaml",
            b"fields:\n  slot: step\n".to_vec(),
        );
        fs.insert(
            "/v/h.md",
            b"---\ntype: host\nslot:\n---\n\n```yaml [:slot]\ntype: step\nref: \"[[fencetarget]]\"\n```\n".to_vec(),
        );
        let kb = build(Path::new("/v"), &fs).unwrap();

        let v = RepoName("v".into());
        let h = PathBuf::from("/v/h.md");
        assert!(
            kb.referenced_names
                .get(&(v, NameKey::Bare("fencetarget".into())))
                .is_some_and(|s| s.contains(&h)),
            "a fence-record field reference must be in the referenced-name index"
        );
    }
}
