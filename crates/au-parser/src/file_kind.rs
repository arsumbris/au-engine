//! Path-and-shape based classification of knowledge base files.
//!
//! Conventions (per [[type-def::au-type-system]] and [[type-instance::au-type-system]]):
//! - A file is a type-def IFF its name ends `.type.yaml` / `.type.yml`. The
//!   suffix is the sole, self-describing marker; location plays no part. A
//!   `type/` directory is an authoring convention surfaced advisorily at the
//!   build layer (`type-def-outside-type-dir`), never a classification rule, so
//!   a prose `README.md` under `type/` is a plain note, not a broken type-def.
//! - Every other file with a top-level `type:` key is an instance. The key
//!   lives in YAML frontmatter for markdown files and at the document root for
//!   pure-YAML instances (`*.yaml` / `*.yml` not classified as type-defs).
//! - Anything else is unclassified (e.g. plain markdown notes with no
//!   frontmatter).
//!
//! Path classification is cheap and runs first; frontmatter classification
//! requires the YAML to be parsed, so the parser passes whatever it knows.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    TypeDef,
    Instance,
    /// `.arsumbris/repo.yaml`, an engine-schema file: a substrate node
    /// whose shape is hardwired in the engine, see
    /// [[spec - engine-schema files - hardwired-schema files are first-class substrate nodes]].
    RepoRegistry,
    /// `.arsumbris/workspace.yaml`, the workspace manifest, an engine-schema file
    /// like the registry. Declares the `members` in scope. Read by targeted path
    /// (it lives under the walk floor), never path-classified, so this kind is
    /// assigned directly when the entry manifest is catalogued.
    Workspace,
    /// `.arsumbris/repo.lock`, the per-repo dependency lock, an engine-written
    /// engine-schema file typed `au.engine.repo-lock`. Under the walk floor, so
    /// it is catalogued by targeted read, not walked.
    RepoLock,
    Unclassified,
}

/// Classify by path alone. Returns `Some(TypeDef)` for known type-def paths,
/// otherwise `None` — the caller must inspect frontmatter to distinguish
/// `Instance` from `Unclassified`.
pub fn classify_by_path(path: &Path) -> Option<FileKind> {
    let name = path.file_name()?.to_string_lossy();
    if name.ends_with(".type.yaml") || name.ends_with(".type.yml") {
        return Some(FileKind::TypeDef);
    }
    None
}

/// True when any component of `path` is exactly `type` (a literal component,
/// never a substring: `prototype/` and `typescript/` do not count).
///
/// `path` should be RELATIVE TO ITS MEMBER ROOT, so a `type` component in the
/// root's own prefix is not miscredited. This backs the advisory `type/`
/// authoring convention (`type-def-outside-type-dir`), never the classification
/// decision, which is the `.type.yaml` suffix alone (see [`classify_by_path`]).
pub fn is_under_type_dir(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_string_lossy() == "type")
}

/// True when the file should be parsed as a single YAML document with no
/// markdown body — i.e. `*.yaml` / `*.yml` that isn't a type-def. Markdown
/// instances are split via `split_frontmatter`; pure-YAML instances go through
/// `whole_as_frontmatter` so a leading `---` (a legitimate YAML document
/// marker) isn't misread as opening a frontmatter region.
pub fn is_pure_yaml_instance_path(path: &Path) -> bool {
    if classify_by_path(path).is_some() {
        return false;
    }
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy();
    name.ends_with(".yaml") || name.ends_with(".yml")
}

/// True when the file is a candidate for instance parsing — `.md`, `.yaml`,
/// or `.yml`, and not a type-def. Everything else the walker returns (PDFs,
/// images, archives, scripts, arbitrary binaries) is an asset: it lives in
/// the `RepoIndex` for `file*` reference resolution, but the parser never
/// reads its bytes, so it can't trigger `repo-file-not-utf8` or any other
/// content diagnostic.
pub fn is_instance_candidate_path(path: &Path) -> bool {
    // A path the engine classifies natively (type-def, workspace manifest) is
    // not an instance candidate.
    if classify_by_path(path).is_some() {
        return false;
    }
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy();
    name.ends_with(".md") || name.ends_with(".yaml") || name.ends_with(".yml")
}

/// Classify by combining path classification with a hint about whether the
/// file's frontmatter declares a top-level `type:` key. `frontmatter_has_type`
/// is `None` when the file has no frontmatter at all.
pub fn classify(path: &Path, frontmatter_has_type: Option<bool>) -> FileKind {
    if let Some(k) = classify_by_path(path) {
        return k;
    }
    match frontmatter_has_type {
        Some(true) => FileKind::Instance,
        _ => FileKind::Unclassified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_yaml_suffix_is_typedef() {
        assert_eq!(
            classify_by_path(Path::new("/v/decision.type.yaml")),
            Some(FileKind::TypeDef)
        );
        assert_eq!(
            classify_by_path(Path::new("/v/decision.type.yml")),
            Some(FileKind::TypeDef)
        );
    }

    #[test]
    fn suffix_classifies_regardless_of_directory() {
        // The `.type.yaml` suffix is the sole marker: it classifies at any
        // depth, inside a `type/` dir or not, and nested subdirs under `type/`
        // still classify because they carry the suffix.
        assert_eq!(
            classify_by_path(Path::new("/v/type/meta/display-meta.type.yaml")),
            Some(FileKind::TypeDef)
        );
        assert_eq!(
            classify_by_path(Path::new("/v/sub/type/a/b/leaf.type.yaml")),
            Some(FileKind::TypeDef)
        );
        // Outside any `type/` dir — still a type-def by suffix (the location is
        // only an advisory convention, checked at the build layer).
        assert_eq!(
            classify_by_path(Path::new("/v/notes/foo.type.yaml")),
            Some(FileKind::TypeDef)
        );
    }

    #[test]
    fn is_under_type_dir_matches_literal_component_only() {
        // Backs the advisory convention: a `type` component (at any depth)
        // counts, a substring does not.
        assert!(is_under_type_dir(Path::new("type/foo.type.yaml")));
        assert!(is_under_type_dir(Path::new("type/meta/foo.type.yaml")));
        assert!(is_under_type_dir(Path::new("a/b/type/foo.type.yaml")));
        assert!(!is_under_type_dir(Path::new("notes/foo.type.yaml")));
        assert!(!is_under_type_dir(Path::new("foo.type.yaml")));
        // substring, not a component
        assert!(!is_under_type_dir(Path::new("prototype/foo.type.yaml")));
        assert!(!is_under_type_dir(Path::new("typescript/foo.type.yaml")));
    }

    #[test]
    fn non_suffixed_file_under_type_dir_is_not_a_typedef() {
        // Dropping the "any `type/` component" rule: a bare `.yaml`, a `.md`,
        // or a prose README under `type/` is no longer force-classified as a
        // type-def. It falls through to the instance / note path instead.
        assert_eq!(classify_by_path(Path::new("/v/type/decision.yaml")), None);
        assert_eq!(classify_by_path(Path::new("/v/sub/type/x.md")), None);
        assert_eq!(classify_by_path(Path::new("/v/type/README.md")), None);
        assert_eq!(classify_by_path(Path::new("/v/type/a/b/c/f.yaml")), None);
    }

    #[test]
    fn other_paths_are_path_unclassified() {
        assert_eq!(classify_by_path(Path::new("/v/notes/a.md")), None);
    }

    #[test]
    fn au_workspace_suffix_is_not_recognized() {
        // The interim `<name>.au-workspace.yaml` form is retired. Such a file is
        // now a plain pure-yaml file, path-unclassified; the one workspace
        // manifest is `.arsumbris/workspace.yaml`, read by targeted path.
        assert_eq!(
            classify_by_path(Path::new("/v/demo.au-workspace.yaml")),
            None
        );
        assert!(is_pure_yaml_instance_path(Path::new(
            "/v/demo.au-workspace.yaml"
        )));
    }

    #[test]
    fn frontmatter_type_promotes_to_instance() {
        assert_eq!(
            classify(Path::new("/v/notes/a.md"), Some(true)),
            FileKind::Instance
        );
    }

    #[test]
    fn pure_yaml_instance_path_recognizes_yaml_extensions() {
        assert!(is_pure_yaml_instance_path(Path::new("/v/good/note.yaml")));
        assert!(is_pure_yaml_instance_path(Path::new("/v/good/note.yml")));
    }

    #[test]
    fn pure_yaml_instance_path_excludes_typedefs() {
        // `*.type.yaml` is a typedef, parsed via its own dispatch, not as a
        // pure-YAML instance. A bare `.yaml` under `type/` is now an ordinary
        // pure-YAML instance candidate (the `type/` rule is gone).
        assert!(!is_pure_yaml_instance_path(Path::new(
            "/v/decision.type.yaml"
        )));
        assert!(is_pure_yaml_instance_path(Path::new("/v/type/x.yaml")));
    }

    #[test]
    fn pure_yaml_instance_path_excludes_markdown_and_other_extensions() {
        assert!(!is_pure_yaml_instance_path(Path::new("/v/good/note.md")));
        assert!(!is_pure_yaml_instance_path(Path::new("/v/good/note.txt")));
        assert!(!is_pure_yaml_instance_path(Path::new("/v/good/noext")));
    }

    #[test]
    fn instance_candidate_path_accepts_md_and_yaml() {
        assert!(is_instance_candidate_path(Path::new("/v/notes/a.md")));
        assert!(is_instance_candidate_path(Path::new("/v/notes/a.yaml")));
        assert!(is_instance_candidate_path(Path::new("/v/notes/a.yml")));
    }

    #[test]
    fn instance_candidate_path_rejects_assets() {
        // PDFs, images, archives, scripts: parser never touches them. The
        // walker still returns them so `RepoIndex` can resolve `file*` refs.
        assert!(!is_instance_candidate_path(Path::new("/v/media/book.pdf")));
        assert!(!is_instance_candidate_path(Path::new("/v/media/photo.png")));
        assert!(!is_instance_candidate_path(Path::new(
            "/v/media/diagram.jpg"
        )));
        assert!(!is_instance_candidate_path(Path::new(
            "/v/scripts/build.sh"
        )));
        assert!(!is_instance_candidate_path(Path::new("/v/data.bin")));
        assert!(!is_instance_candidate_path(Path::new("/v/noext")));
    }

    #[test]
    fn instance_candidate_path_rejects_only_suffixed_typedefs() {
        // `*.type.yaml` is a typedef path; the build handles it in its type-def
        // pass, not the instance loop. A bare `.yaml` / `.md` under `type/` is
        // now an ordinary instance candidate (the `type/` rule is gone).
        assert!(!is_instance_candidate_path(Path::new(
            "/v/decision.type.yaml"
        )));
        assert!(is_instance_candidate_path(Path::new("/v/type/x.yaml")));
        assert!(is_instance_candidate_path(Path::new("/v/type/x.md")));
    }

    #[test]
    fn no_frontmatter_or_no_type_is_unclassified() {
        assert_eq!(
            classify(Path::new("/v/notes/a.md"), Some(false)),
            FileKind::Unclassified
        );
        assert_eq!(
            classify(Path::new("/v/notes/a.md"), None),
            FileKind::Unclassified
        );
    }
}
