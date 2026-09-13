//! An overlay `FileSystem` for the mutation preview read: an inner filesystem
//! with ONE path overridden, either replaced with would-be bytes (a write or an
//! edit's product) or removed (a delete). The engine's rebuild dispatch reads a
//! dirty path through the `FileSystem` port, so running that dispatch over this
//! overlay computes a mutation's product without writing disk or touching git.
//! See [[spec - mutation preview read - simulate a write over an overlay and
//! report its product without committing]].

use std::io;
use std::path::{Path, PathBuf};

use au_parser::scope::WalkFilter;
use au_parser::{FileSystem, ScopeBoundaries, Walk, WalkError};

/// The one-path change an overlay applies over its inner filesystem.
pub(crate) enum Override {
    /// A write or an edit product: the path reads as `content`, and an in-scope
    /// add joins the walk.
    Replace { path: PathBuf, content: Vec<u8> },
    /// A delete: the path reads as absent, and the walk excludes it.
    Remove { path: PathBuf },
}

impl Override {
    fn path(&self) -> &Path {
        match self {
            Override::Replace { path, .. } | Override::Remove { path } => path,
        }
    }
}

/// A `FileSystem` presenting its inner filesystem with one path overridden. All
/// other paths fall through to the inner, so a preview reuses every unchanged
/// file's real bytes and re-parses only the overridden one.
pub(crate) struct OverlayFileSystem<'a, F: FileSystem> {
    inner: &'a F,
    over: Override,
}

impl<'a, F: FileSystem> OverlayFileSystem<'a, F> {
    pub(crate) fn new(inner: &'a F, over: Override) -> Self {
        Self { inner, over }
    }
}

impl<F: FileSystem> FileSystem for OverlayFileSystem<'_, F> {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        if path == self.over.path() {
            return match &self.over {
                Override::Replace { content, .. } => Ok(content.clone()),
                Override::Remove { .. } => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    path.display().to_string(),
                )),
            };
        }
        self.inner.read_file(path)
    }

    fn is_file(&self, path: &Path) -> bool {
        if path == self.over.path() {
            return matches!(self.over, Override::Replace { .. });
        }
        self.inner.is_file(path)
    }

    /// The overridden path reports its override's size (a `Replace`'s content
    /// length, or absent for a `Remove`); every other path delegates to the
    /// inner filesystem's cheap stat rather than the read-based default, so a
    /// preview honours the read cap exactly as a real build does.
    fn file_len(&self, path: &Path) -> io::Result<u64> {
        if path == self.over.path() {
            return match &self.over {
                Override::Replace { content, .. } => Ok(content.len() as u64),
                Override::Remove { .. } => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    path.display().to_string(),
                )),
            };
        }
        self.inner.file_len(path)
    }

    fn walk_files(&self, root: &Path, filter: &WalkFilter) -> io::Result<Walk> {
        let mut walk = self.inner.walk_files(root, filter)?;
        match &self.over {
            // A write ADD: a new in-scope path the inner walk did not yield joins
            // the file set. An overwrite or an edit is already present, so the
            // guard leaves it untouched. The hard-floor exclusion is upstream: the
            // mutation path rejects an `.arsumbris` target before a preview ever
            // reaches this overlay.
            Override::Replace { path, .. } => {
                if path.starts_with(root)
                    && filter.keep_file(path)
                    && !walk.files.iter().any(|p| p == path)
                {
                    walk.files.push(path.clone());
                }
            }
            // A delete: the removed path leaves the file set.
            Override::Remove { path } => {
                walk.files.retain(|p| p != path);
            }
        }
        Ok(walk)
    }

    fn walk_scope_boundaries(
        &self,
        root: &Path,
        filter: &WalkFilter,
    ) -> io::Result<(ScopeBoundaries, Vec<WalkError>)> {
        // A single content override does not move the scope boundaries the ignore
        // patterns decide, so the inner view is faithful.
        self.inner.walk_scope_boundaries(root, filter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use au_parser::MemoryFileSystem;

    fn base() -> MemoryFileSystem {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/a.md", b"aaa".to_vec());
        fs.insert("/v/b.md", b"bbb".to_vec());
        fs
    }

    #[test]
    fn replace_overrides_read_and_is_file() {
        let inner = base();
        let over = Override::Replace {
            path: PathBuf::from("/v/a.md"),
            content: b"NEW".to_vec(),
        };
        let fs = OverlayFileSystem::new(&inner, over);
        // The overridden path reads the would-be bytes; others fall through.
        assert_eq!(fs.read_file(Path::new("/v/a.md")).unwrap(), b"NEW");
        assert_eq!(fs.read_file(Path::new("/v/b.md")).unwrap(), b"bbb");
        assert!(fs.is_file(Path::new("/v/a.md")));
    }

    #[test]
    fn replace_of_a_new_path_is_readable_and_a_file() {
        let inner = base();
        let over = Override::Replace {
            path: PathBuf::from("/v/c.md"),
            content: b"CCC".to_vec(),
        };
        let fs = OverlayFileSystem::new(&inner, over);
        assert_eq!(fs.read_file(Path::new("/v/c.md")).unwrap(), b"CCC");
        assert!(fs.is_file(Path::new("/v/c.md")));
    }

    #[test]
    fn remove_hides_the_path_from_read_and_is_file() {
        let inner = base();
        let over = Override::Remove {
            path: PathBuf::from("/v/a.md"),
        };
        let fs = OverlayFileSystem::new(&inner, over);
        assert_eq!(
            fs.read_file(Path::new("/v/a.md")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(!fs.is_file(Path::new("/v/a.md")));
        // A sibling is untouched.
        assert!(fs.is_file(Path::new("/v/b.md")));
    }

    #[test]
    fn walk_injects_an_add_and_passes_an_edit_through() {
        let inner = base();
        let root = Path::new("/v");
        let filter = WalkFilter::default_excludes(root);
        // A brand-new in-scope path joins the walk.
        let add = OverlayFileSystem::new(
            &inner,
            Override::Replace {
                path: PathBuf::from("/v/c.md"),
                content: b"CCC".to_vec(),
            },
        );
        let files = add.walk_files(root, &filter).unwrap().files;
        assert!(files.iter().any(|p| p == Path::new("/v/c.md")));
        assert_eq!(files.len(), 3);
        // An edit of an existing path does not duplicate it.
        let edit = OverlayFileSystem::new(
            &inner,
            Override::Replace {
                path: PathBuf::from("/v/a.md"),
                content: b"NEW".to_vec(),
            },
        );
        let files = edit.walk_files(root, &filter).unwrap().files;
        assert_eq!(
            files.iter().filter(|p| *p == Path::new("/v/a.md")).count(),
            1
        );
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn walk_drops_a_removed_path() {
        let inner = base();
        let root = Path::new("/v");
        let filter = WalkFilter::default_excludes(root);
        let fs = OverlayFileSystem::new(
            &inner,
            Override::Remove {
                path: PathBuf::from("/v/a.md"),
            },
        );
        let files = fs.walk_files(root, &filter).unwrap().files;
        assert!(!files.iter().any(|p| p == Path::new("/v/a.md")));
        assert!(files.iter().any(|p| p == Path::new("/v/b.md")));
        assert_eq!(files.len(), 1);
    }
}
