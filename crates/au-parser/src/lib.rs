//! Filesystem port, frontmatter splitter, markdown body scanner, YAML AST with source spans, file-kind classifier.
//!
//! This crate is the engine's only point of contact with bytes-on-disk. Every
//! other crate consumes parsed structures, never raw I/O.

pub mod body;
pub mod codes;
pub mod file_kind;
pub mod frontmatter;
pub mod fs;
pub mod scope;
pub mod yaml;

pub use body::{
    derive_section_paths, is_valid_block_id, scan_body, scan_wikilink_spans,
    take_wikilink_scan_steps, BodyEvent,
};
pub use codes::{
    AUIGNORE_EMPTY_SCOPE, AUIGNORE_LOAD_ERROR, DUPLICATE_KEY_IN_MAPPING, FILE_TOO_LARGE,
    FRONTMATTER_UNTERMINATED, REPO_FILE_NOT_UTF8, REPO_FILE_READ_ERROR, REPO_WALK_ERROR,
    YAML_PARSE_ERROR,
};
pub use file_kind::{
    classify, classify_by_path, is_instance_candidate_path, is_pure_yaml_instance_path,
    is_under_type_dir, FileKind,
};
pub use frontmatter::{
    split_frontmatter, whole_as_frontmatter, FrontmatterError, FrontmatterSplit,
};
pub use fs::{FileSystem, MemoryFileSystem, RealFileSystem, ScopeBoundaries, Walk, WalkError};
pub use scope::{WalkFilter, DEFAULT_EXCLUDE_DIR_NAMES, FLOOR_DIR_NAMES};
pub use yaml::{
    index_source, parse, scan_duplicate_keys, span_to_byte_range, take_cp_fallback_count,
    DuplicateKey, MarkedYaml, SourceIndexGuard, YamlData, YamlError,
};
