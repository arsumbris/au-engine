//! Integration coverage for the held build over a real scenario repo.
//!
//! Exercises the parse-layer/resolved-layer structure directly: every file is
//! catalogued with its parse, type-defs feed the graph, instances carry their
//! parse and resolved view, and a rebuild is deterministic.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use au_engine::{build, build_reusing, BuildOutcome, ContentHash, FileParse, UserRegistry};
use au_parser::{FileKind, RealFileSystem};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn build_catalogs_every_file_with_its_parse() {
    let repo = workspace_root().join("scenarios/body-section-clean");
    let v = build(&repo, &RealFileSystem).expect("build");

    assert_eq!(
        v.root_outcome(),
        BuildOutcome::Complete,
        "clean scenario completes"
    );
    assert!(!v.catalog.is_empty(), "catalog holds the walked files");
    assert!(
        v.root_graph().len() >= 1,
        "type graph built from the type-def"
    );

    // The type-def parsed into a FileParse::TypeDef carrying its type-def.
    let type_defs = v
        .catalog
        .values()
        .filter(|e| e.kind == FileKind::TypeDef)
        .count();
    assert_eq!(type_defs, 1, "one type-def file catalogued");
    assert!(
        v.catalog.values().any(|e| matches!(
            e.parse.as_ref(),
            FileParse::TypeDef {
                type_def: Some(_),
                ..
            }
        )),
        "type-def file holds a parsed type-def"
    );

    // The instance parsed into a FileParse::Instance, and a resolved view with
    // an effective shape.
    let instance_path = repo.join("migrate-sqlite.md");
    match v.file_parse(&instance_path) {
        Some(FileParse::Instance {
            instance: Some(_), ..
        }) => {}
        other => panic!("expected an instance parse, got {other:?}"),
    }
    let resolved = v
        .instances
        .get(&instance_path)
        .expect("resolved view for the instance");
    assert!(
        resolved.effective_shape.is_some(),
        "instance claim resolves to a shape"
    );

    // Every catalogued instance has a content hash (it was read).
    for entry in v.catalog.values() {
        if entry.kind == FileKind::Instance || entry.kind == FileKind::TypeDef {
            assert!(entry.hash.is_some(), "read files carry a content hash");
        }
    }
}

#[test]
fn rebuild_is_deterministic() {
    let repo = workspace_root().join("scenarios/body-section-clean");
    let a = build(&repo, &RealFileSystem).expect("build a");
    let b = build(&repo, &RealFileSystem).expect("build b");

    assert!(
        a.diagnostics().eq(b.diagnostics()),
        "diagnostics are stable"
    );
    assert_eq!(a.catalog.size(), b.catalog.size(), "catalog is stable");
    // Content hashes are deterministic across rebuilds — the seed of reusing
    // an unchanged file's parse.
    for (path, entry) in &a.catalog {
        assert_eq!(
            entry.hash,
            b.catalog.get(path).unwrap().hash,
            "hash stable for {path:?}"
        );
    }
}

// A read file's recorded byte length equals the bytes it was built from; an
// unread asset carries none. The size a consumer costs a fetch against, so it
// must agree with what `content` would return and must never be a phantom zero.
#[test]
fn byte_len_matches_content_and_is_none_for_assets() {
    let repo = workspace_root().join("scenarios/asset-binary-ignored");
    let v = build(&repo, &RealFileSystem).expect("build");

    // The invariant: byte_len is Some exactly when hash is, across the catalog.
    for (path, entry) in v.catalog.iter() {
        assert_eq!(
            entry.byte_len.is_some(),
            entry.hash.is_some(),
            "byte_len and hash are Some together for {}",
            path.display()
        );
    }

    // A read type-def and a read instance carry the exact byte length of their
    // on-disk content — what `content`'s `text` serves for them.
    for name in ["type/note.type.yaml", "note.md"] {
        let path = repo.join(name);
        let entry = v.catalog.get(&path).expect("read file catalogued");
        let disk = std::fs::read(&path).expect("read fixture");
        assert_eq!(
            entry.byte_len,
            Some(disk.len()),
            "{name} byte_len equals its content length"
        );
    }

    // The PDF is catalogued but never read, so it cannot be costed: None, not 0.
    let pdf = repo.join("media/book.pdf");
    let asset = v.catalog.get(&pdf).expect("asset catalogued");
    assert_eq!(asset.hash, None, "the asset is unread");
    assert_eq!(asset.byte_len, None, "an unread asset has no costable size");
}

// A read-skipping incremental rebuild reuses the parse layer, which carries the
// byte length forward without re-reading the bytes. Proven by pointer-identical
// parses (the read was skipped) alongside an unchanged byte_len.
#[test]
fn byte_len_survives_a_read_skipping_rebuild() {
    let repo = workspace_root().join("scenarios/asset-binary-ignored");
    let base = build(&repo, &RealFileSystem).expect("build");

    let prior = base.parse_layer();
    let known: BTreeMap<PathBuf, Option<ContentHash>> = base
        .catalog
        .iter()
        .map(|(p, e)| (p.clone(), e.hash))
        .collect();
    let reused = build_reusing(
        &repo,
        &RealFileSystem,
        &UserRegistry::new(),
        &prior,
        &known,
        None,
    )
    .expect("reuse build");

    let path = repo.join("note.md");
    let base_entry = base.catalog.get(&path).expect("base entry");
    let reused_entry = reused.catalog.get(&path).expect("reused entry");
    assert!(
        Arc::ptr_eq(&base_entry.parse, &reused_entry.parse),
        "the parse was reused, so the bytes were not re-read"
    );
    assert_eq!(
        reused_entry.byte_len, base_entry.byte_len,
        "byte_len rides the reused parse layer, not a fresh read"
    );
    assert!(
        reused_entry.byte_len.is_some(),
        "a read file carries a length"
    );
}
