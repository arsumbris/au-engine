# au-parser

The engine's only point of contact with bytes-on-disk. Other crates consume parsed structures, never raw I/O.

## Modules

- `fs` — `FileSystem` port; `RealFileSystem` (std::fs), `MemoryFileSystem` (BTreeMap-backed test double). `walk_files` returns every regular file under the root regardless of extension, skipping `.git`, `node_modules`, `target`, `.arsumbris`. Iterative walk follows symlinks with cycle detection (canonical-path dedup) and caps at 64 levels deep. au-references' `RepoIndex` consumes the output directly so `file*` references find PDFs / images / arbitrary binaries.
- `frontmatter` — `split_frontmatter` for `---/---/body` files (CRLF-aware, leading UTF-8 BOM stripped). Returns `Result<Option<_>, FrontmatterError>`: `Ok(None)` for files with no frontmatter, `Err(Unterminated)` for files that opened with `---` but never closed (pre-fix this collapsed silently into `None`). `whole_as_frontmatter` for pure-YAML type-def files.
- `yaml` — `parse(text)` returns saphyr's `MarkedYaml`. `span_to_byte_range(source, yaml_offset, span)` converts saphyr's codepoint-indexed spans into file-relative byte ranges (saphyr counts Unicode scalars, not bytes — multi-byte UTF-8 needs `char_indices` to land in the right place).
- `file_kind` — `classify_by_path` (cheap, path-only) and `classify` (path + frontmatter-has-`type:` hint).
- `body` — markdown body scanner per body-typing v5. `scan_body(src) → Vec<BodyEvent>` walks the bytes after the frontmatter split and emits typed events: `Heading { level, text }`, `FencedBlock { info, body, trailing_block_id }`, `InlineCode { content }`, `Wikilink { raw }`, `BlockIdMarker { id }`. Single linear pass + a second wikilink pass that excludes regions covered by fences / inline-code / headings. Multi-line wikilinks supported; backslash-escaped `\[[` skipped. Events emit in source order across all kinds. `derive_section_paths(events) → Vec<(Vec<String>, &BodyEvent)>` produces root-to-leaf paths (`"<1-based-index-at-level> <heading text>"`) per event, with sibling counters scoped per parent.

Body-text wiki-link **typed-contribution** resolution (matching `:field` fragments to slot shapes) lives in au-core's body_validate; this crate just emits the structural events.
