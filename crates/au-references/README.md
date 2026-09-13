# au-references

Wiki-link parsing + repo file index + reference resolution. Used by the validator for slot-reference checks (`name*`, `file*`).

## Surface

- `parse_wikilink(s) → Result<WikilinkRef, WikilinkParseError>` — recognizes `[[target[#anchor][^block-id][:field]]]`. Strict parse order; reverse delimiters and field-fragment misplacement rejected. `:field` value validated against the type-name regex (lifted into the crate to avoid an au-grammar dep). Anchors and block-ids parse-and-strip; the `:field` fragment carries body-typing v5 attribution.
- `RepoIndex::build(root, files) → (RepoIndex, Vec<Diagnostic>)` — basename + relative-path index. Detects `case-collision-basename` at construction and returns the diagnostics alongside the index.
- `RepoIndex::resolve(target) → Result<PathBuf, ResolutionError>` — `/`-bearing targets resolve as repo-relative paths; bare targets match basenames case-insensitively. An extensionless target matches any file's stem, whatever the extension (`s-001` reaches `s-001.yaml`); an extension is only required when the stem alone is ambiguous. Multiple matches → `Ambiguous`; none → `Missing`.
- `resolve_block_id(events, id) → Result<ResolvedBlock, BlockResolutionError>` — walks a target file's scanned `BodyEvent`s and finds the typed fenced block carrying `^id`. Returns the block's info string + body, or `NotFound` / `NotTyped`. Powers `[[file^id]]` references that target marked-fence contributions.
- `extract_field_marker(info) → Option<&str>` — pulls the `:fieldName` out of a fence info string (e.g. `"yaml [:assumptions]"` → `Some("assumptions")`).

## Codes

`au-references::codes` exports kebab-case `DiagnosticCode` constants:

- `case-collision-basename` (Warning) — two basenames collide modulo case; legal on Linux, illegal on macOS-default APFS.
- `reference-target-missing` (Error) — wikilink resolved nothing.
- `reference-target-ambiguous` (Error) — basename matches multiple files.
- `reference-target-type-mismatch` (Error) — produced by au-core; lives here so all reference-related codes co-locate.
- `wikilink-empty-target` / `wikilink-empty-anchor` / `wikilink-empty-block-id` / `wikilink-empty-field` (Error) — delimiter with no value following.
- `wikilink-reversed-delimiters` (Error) — `^` before `#`.
- `wikilink-fragment-order` (Error) — `:field` placed before `#anchor` / `^block-id`.
- `wikilink-invalid-field-name` (Error) — `:field` value violates the field-name regex.

au-parser is a build-time dep so the block-id resolver can walk `BodyEvent`s from target files. No filesystem I/O — knowledge base index built from a caller-supplied path list; body events come from caller-supplied scans.
