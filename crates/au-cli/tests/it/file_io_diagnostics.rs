//! Per-file I/O failures (unreadable bytes, non-UTF-8, walk errors) must
//! surface as diagnostics, not silent drops. These are the engine's own
//! behavior — `au_engine::build` owns the single walk-read-parse loop — so the
//! tests drive `build` directly against an in-memory file system.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use au_engine::{build, KnowledgeBase};
use au_parser::{FileSystem, ScopeBoundaries, WalkError, WalkFilter};

#[derive(Debug, Default)]
struct FakeFs {
    files: BTreeMap<PathBuf, Vec<u8>>,
    unreadable: BTreeSet<PathBuf>,
}

impl FakeFs {
    fn new() -> Self {
        let mut fs = Self::default();
        // Every knowledge base entry must be a folder-repo. `/v` (basename `v`) carries a
        // matching `name: v`, so no folder-name-mismatch drift.
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs
    }

    fn insert(&mut self, path: impl Into<PathBuf>, content: impl Into<Vec<u8>>) {
        self.files.insert(path.into(), content.into());
    }

    /// Register a path that `walk_files` will list but `read_file` will
    /// reject with PermissionDenied — models a file the walker enumerated
    /// before someone revoked read access (or between the listdir and the
    /// open call).
    fn insert_unreadable(&mut self, path: impl Into<PathBuf>) {
        let p = path.into();
        self.files.insert(p.clone(), Vec::new());
        self.unreadable.insert(p);
    }
}

impl FileSystem for FakeFs {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        if self.unreadable.contains(path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "fake: permission denied",
            ));
        }
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.display().to_string()))
    }

    /// Presence, not readability: an unreadable file is still present, and the
    /// walk still lists it.
    fn is_file(&self, path: &Path) -> bool {
        self.files.contains_key(path)
    }

    fn walk_files(&self, root: &Path, filter: &WalkFilter) -> io::Result<au_parser::Walk> {
        let files: Vec<PathBuf> = self
            .files
            .keys()
            .filter(|p| p.starts_with(root) && filter.keep_file(p))
            .cloned()
            .collect();
        Ok(au_parser::Walk {
            files,
            repo_markers: Vec::new(),
            errors: Vec::new(),
        })
    }

    fn walk_scope_boundaries(
        &self,
        root: &Path,
        filter: &WalkFilter,
    ) -> io::Result<(ScopeBoundaries, Vec<WalkError>)> {
        // Reconstruct boundaries from the flat map, as `MemoryFileSystem` does:
        // the shallowest pruned directory ancestor is one boundary, else a
        // dropped file is one entry.
        let mut ignored_dirs: BTreeSet<PathBuf> = BTreeSet::new();
        let mut ignored_files: BTreeSet<PathBuf> = BTreeSet::new();
        for key in self.files.keys() {
            let Ok(rel) = key.strip_prefix(root) else {
                continue;
            };
            let comps: Vec<_> = rel.components().collect();
            let mut dir = root.to_path_buf();
            let mut boundary = None;
            for comp in &comps[..comps.len().saturating_sub(1)] {
                dir.push(comp);
                if !filter.enter_dir(&dir) {
                    boundary = Some(dir.clone());
                    break;
                }
            }
            match boundary {
                Some(d) => {
                    ignored_dirs.insert(d);
                }
                None => {
                    if !filter.keep_file(key) {
                        ignored_files.insert(key.clone());
                    }
                }
            }
        }
        Ok((
            ScopeBoundaries {
                ignored_dirs: ignored_dirs.into_iter().collect(),
                ignored_files: ignored_files.into_iter().collect(),
            },
            Vec::new(),
        ))
    }
}

fn codes_of(kb: &KnowledgeBase) -> Vec<&str> {
    kb.diagnostics().map(|d| d.code.as_str()).collect()
}

fn has_type(kb: &KnowledgeBase, name: &str) -> bool {
    kb.root_graph().iter().any(|(n, _)| n.as_str() == name)
}

/// Wraps an inner `FakeFs` and tallies `walk_files` invocations. Pins the
/// "build walks the knowledge base once" invariant — a regression here means the build
/// re-walked instead of reusing the file list from its single walk.
#[derive(Debug)]
struct CountingFs {
    inner: FakeFs,
    walks: Cell<usize>,
}

impl CountingFs {
    fn new(inner: FakeFs) -> Self {
        Self {
            inner,
            walks: Cell::new(0),
        }
    }

    fn walks(&self) -> usize {
        self.walks.get()
    }
}

impl FileSystem for CountingFs {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read_file(path)
    }

    fn is_file(&self, path: &Path) -> bool {
        self.inner.is_file(path)
    }

    fn walk_files(&self, root: &Path, filter: &WalkFilter) -> io::Result<au_parser::Walk> {
        self.walks.set(self.walks.get() + 1);
        self.inner.walk_files(root, filter)
    }

    fn walk_scope_boundaries(
        &self,
        root: &Path,
        filter: &WalkFilter,
    ) -> io::Result<(ScopeBoundaries, Vec<WalkError>)> {
        self.inner.walk_scope_boundaries(root, filter)
    }
}

// CountingFs accesses Cell from `&self`. Cell isn't Sync, but the FileSystem
// trait requires Sync; tests are single-threaded, so the access is safe.
unsafe impl Sync for CountingFs {}

// ---- type-def reads -------------------------------------------------------

#[test]
fn unreadable_typedef_surfaces_a_read_error() {
    let mut fs = FakeFs::new();
    fs.insert(
        "/v/good.type.yaml",
        b"fields:\n  description: String\n".to_vec(),
    );
    fs.insert_unreadable("/v/bad.type.yaml");

    let kb = build(Path::new("/v"), &fs).unwrap();

    let codes = codes_of(&kb);
    assert!(
        codes.contains(&"repo-file-read-error"),
        "expected repo-file-read-error among {codes:?}"
    );
    assert!(
        kb.diagnostics()
            .any(|d| d.span.file == PathBuf::from("/v/bad.type.yaml")
                && d.code.as_str() == "repo-file-read-error"),
        "expected the bad path on the read-error span; got {:?}",
        kb.diagnostics().collect::<Vec<_>>()
    );
    assert!(has_type(&kb, "good"), "good.type.yaml should still load");
}

#[test]
fn non_utf8_typedef_surfaces_an_encoding_error() {
    let mut fs = FakeFs::new();
    fs.insert(
        "/v/good.type.yaml",
        b"fields:\n  description: String\n".to_vec(),
    );
    // 0xFF is invalid as a UTF-8 lead byte.
    fs.insert("/v/bad.type.yaml", vec![0xFFu8, 0xFE, 0xFD]);

    let kb = build(Path::new("/v"), &fs).unwrap();

    assert!(
        codes_of(&kb).contains(&"repo-file-not-utf8"),
        "expected repo-file-not-utf8 among {:?}",
        codes_of(&kb)
    );
    assert!(has_type(&kb, "good"), "good.type.yaml should still load");
}

// ---- instance reads -------------------------------------------------------

#[test]
fn unreadable_instance_surfaces_a_read_error_without_aborting() {
    let mut fs = FakeFs::new();
    fs.insert(
        "/v/note.type.yaml",
        b"fields:\n  description: String\n".to_vec(),
    );
    fs.insert(
        "/v/good.md",
        b"---\ntype: note\ndescription: ok\n---\n".to_vec(),
    );
    fs.insert_unreadable("/v/bad.md");

    let kb = build(Path::new("/v"), &fs).unwrap();

    assert!(
        codes_of(&kb).contains(&"repo-file-read-error"),
        "expected repo-file-read-error among {:?}",
        codes_of(&kb)
    );
    assert!(
        !kb.any_aborted(),
        "an instance read error is not a load-phase abort"
    );
    assert!(!kb.instances.is_empty(), "the good instance is validated");
}

#[test]
fn non_utf8_instance_surfaces_an_encoding_error() {
    let mut fs = FakeFs::new();
    fs.insert(
        "/v/note.type.yaml",
        b"fields:\n  description: String\n".to_vec(),
    );
    fs.insert(
        "/v/good.md",
        b"---\ntype: note\ndescription: ok\n---\n".to_vec(),
    );
    fs.insert("/v/bad.md", vec![0xFFu8, 0xFE]);

    let kb = build(Path::new("/v"), &fs).unwrap();

    assert!(
        codes_of(&kb).contains(&"repo-file-not-utf8"),
        "expected repo-file-not-utf8 among {:?}",
        codes_of(&kb)
    );
    assert!(!kb.instances.is_empty(), "the good instance is validated");
}

// ---- walk errors ----------------------------------------------------------

#[cfg(unix)]
#[test]
fn broken_symlink_surfaces_a_walk_error() {
    // RealFileSystem against a tempdir — FakeFs models read failures, not walk
    // failures. A per-entry walk error (broken symlink) must reach the user as
    // a `repo-walk-error` diagnostic.
    let tmp = tempfile::tempdir().unwrap();
    // The entry must be a folder-repo.
    std::fs::create_dir_all(tmp.path().join(".arsumbris")).unwrap();
    std::fs::write(tmp.path().join(".arsumbris/repo.yaml"), b"name: v\n").unwrap();
    std::fs::write(
        tmp.path().join("good.type.yaml"),
        b"fields:\n  description: String\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        tmp.path().join("missing-target"),
        tmp.path().join("broken-link.type.yaml"),
    )
    .unwrap();

    let kb = build(tmp.path(), &au_parser::RealFileSystem).unwrap();
    assert!(
        codes_of(&kb).contains(&"repo-walk-error"),
        "expected repo-walk-error among {:?}",
        codes_of(&kb)
    );
    assert!(has_type(&kb, "good"), "valid typedef must still load");
}

// ---- duplicate keys -------------------------------------------------------

#[test]
fn duplicate_top_level_keys_on_instance_are_flagged() {
    // Real-world bug: a user wrote `fooField` twice in instance frontmatter;
    // saphyr's `LinkedHashMap` silently kept the last value and dropped the
    // first. The duplicate-keys scan surfaces it.
    let mut fs = FakeFs::new();
    fs.insert(
        "/v/foo.type.yaml",
        b"fields:\n  fooField: String\n".to_vec(),
    );
    fs.insert(
        "/v/dup.md",
        b"---\ntype: foo\nfooField: first\nfooField: second\n---\n".to_vec(),
    );

    let kb = build(Path::new("/v"), &fs).unwrap();
    assert!(
        codes_of(&kb).contains(&"duplicate-key-in-mapping"),
        "expected duplicate-key-in-mapping; got {:?}",
        codes_of(&kb)
    );
}

#[test]
fn duplicate_keys_inside_an_inline_value_are_flagged() {
    let mut fs = FakeFs::new();
    fs.insert(
        "/v/host.type.yaml",
        b"fields:\n  rationale: rationale\n".to_vec(),
    );
    fs.insert(
        "/v/rationale.type.yaml",
        b"fields:\n  description: String\n".to_vec(),
    );
    fs.insert(
        "/v/dup-inline.md",
        b"---\ntype: host\nrationale:\n  type: rationale\n  description: a\n  description: b\n---\n"
            .to_vec(),
    );

    let kb = build(Path::new("/v"), &fs).unwrap();
    assert!(
        codes_of(&kb).contains(&"duplicate-key-in-mapping"),
        "expected duplicate-key-in-mapping; got {:?}",
        codes_of(&kb)
    );
}

// ---- asset binaries are not parsed ---------------------------------------

/// Real-world bug: a knowledge base with PDFs under `media/` fired `repo-file-not-utf8`
/// against the PDF binary header. Asset binaries the walker returns for
/// `RepoIndex` resolution must not be UTF-8 decoded or YAML-parsed — only
/// `.md` / `.yaml` / `.yml` files are instance candidates — yet they stay in
/// the catalog for `file*` resolution.
#[test]
fn asset_binaries_are_not_parsed_but_stay_catalogued() {
    let mut fs = FakeFs::new();
    fs.insert(
        "/v/note.type.yaml",
        b"fields:\n  description: String\n".to_vec(),
    );
    fs.insert(
        "/v/good.md",
        b"---\ntype: note\ndescription: ok\n---\n".to_vec(),
    );
    // `%PDF-1.7` is ASCII, but the body has 0xFF sequences a few bytes in.
    let mut pdf = b"%PDF-1.7\n".to_vec();
    pdf.extend_from_slice(&[0xFFu8, 0xFE, 0xFD, 0xFC]);
    fs.insert("/v/media/patterns/Pragmatic Programmer.pdf", pdf);
    fs.insert("/v/media/photo.png", vec![0x89, b'P', b'N', b'G', 0x0Du8]);

    let kb = build(Path::new("/v"), &fs).unwrap();

    let codes = codes_of(&kb);
    assert!(
        !codes.contains(&"repo-file-not-utf8"),
        "asset binaries must not trigger repo-file-not-utf8; got {codes:?}"
    );
    assert!(
        !codes.contains(&"yaml-parse-error"),
        "asset binaries must not trigger yaml-parse-error; got {codes:?}"
    );
    assert!(!kb.instances.is_empty(), "good.md is still validated");
    // PDF + PNG + good.md + note.type.yaml + the folder-repo's .arsumbris/repo.yaml
    // node = 5 catalogued files.
    assert_eq!(
        kb.catalog.size(),
        5,
        "asset files must remain catalogued for file* resolution"
    );
}

// ---- bounded-walk invariant -----------------------------------------------

#[test]
fn build_walks_a_single_repo_entry_once() {
    // A single-repo folder-repo entry is walked EXACTLY once. The walk-resolve
    // fixpoint walks the entry as its first (and only) member, surfacing its own
    // marker; resolution mounts just the entry, whose walk is already done, so the
    // fixpoint converges without a re-walk. This restored the single-walk invariant
    // the folder-repo flip had briefly broken (a probe walk plus a member walk).
    let mut inner = FakeFs::new();
    inner.insert(
        "/v/note.type.yaml",
        b"fields:\n  description: String\n".to_vec(),
    );
    inner.insert(
        "/v/inst.md",
        b"---\ntype: note\ndescription: ok\n---\n".to_vec(),
    );
    let fs = CountingFs::new(inner);
    let _kb = build(Path::new("/v"), &fs).unwrap();
    assert_eq!(
        fs.walks(),
        1,
        "one entry walk, reused as the entry member; observed {}",
        fs.walks()
    );
}
