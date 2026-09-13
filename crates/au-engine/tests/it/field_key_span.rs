//! The `type` read surfaces each field's `key_span`, the byte span of the field
//! name in the owning type-def's source, for editor go-to-def onto the exact
//! declaration line.

use std::fs;
use std::path::Path;

use au_engine::build;
use au_parser::RealFileSystem;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

#[test]
fn field_key_span_lands_on_the_field_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: proj\n");
    // Two fields, so the span must land on the SECOND field's name, not the
    // type-def opening line.
    write(
        &root,
        "type/note.type.yaml",
        "fields:\n  description: String\n  count: Number\n",
    );

    let kb = build(&root, &RealFileSystem).expect("build");
    let view = au_engine::wire::introspect_type_by_name(&kb, "note").expect("note type");
    let field = view
        .def
        .fields
        .iter()
        .find(|f| f.name == "count")
        .expect("count field");
    let span = field
        .key_span
        .as_ref()
        .expect("key_span present on the type read");

    // The span is file-relative: the bytes at it are exactly the field name.
    let bytes = fs::read(&view.def.source.file).expect("read type-def source");
    assert_eq!(
        &bytes[span.start..span.end],
        b"count",
        "key_span lands on the field name, file-relative"
    );
    assert!(
        span.line_col.is_some(),
        "line_col is populated for editor navigation"
    );
}
