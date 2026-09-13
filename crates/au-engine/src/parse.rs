//! The per-file parse layer: one run over the parser, bundling owned results.
//!
//! [`parse_file`] is the single entry point for turning a file's bytes into a
//! parse. It does the UTF-8 check, the frontmatter split, one YAML parse plus
//! one duplicate-key scan, and the structural parse into a type-def or
//! instance.
//!
//! The result is a [`FileParse`] of owned data, no borrows of the source, so
//! it can be held and reused while the file's content hash is unchanged. It
//! depends only on the bytes and the path (for classification and span
//! attribution), not on the type graph or any other file, the branch-agnostic
//! parse layer of [[design - engine shape]].

use std::path::{Path, PathBuf};

use au_core::{
    parse_instance, parse_instance_stamped, parse_note, parse_type_def, DocstringLink, Instance,
    InstanceField, TypeDef, TypeNameClaim,
};
use au_diagnostics::{ByteRange, Diagnostic, Severity, Span, SuggestedFix};
use au_parser::{
    classify_by_path, is_instance_candidate_path, is_pure_yaml_instance_path, split_frontmatter,
    whole_as_frontmatter,
    yaml::{parse, Scalar, YamlData},
    FileKind, FrontmatterError, MarkedYaml, FRONTMATTER_UNTERMINATED, YAML_PARSE_ERROR,
};

use crate::diagnostics::{duplicate_key_diags, not_utf8_diag};

/// The parse-layer result for one file.
///
/// Owned and source-free, so it survives across rebuilds while the file's
/// content hash holds. The variant follows the file's kind; `Unparsed` covers
/// notes without a `type:`, files whose frontmatter or YAML failed, and any
/// non-candidate the caller routes here.
#[derive(Debug, Clone)]
pub enum FileParse {
    /// A type-def file. `type_def` is `None` when the structural parse failed.
    TypeDef {
        type_def: Option<TypeDef>,
        /// Navigational links from the def's `#:` docstrings (head, fields, and
        /// meta blocks), each tagged with its binding declaration. See
        /// [[spec - docstring navigational links - a docstring's wikilinks resolve as navigational edges tagged to their declaration]].
        doc_links: Vec<DocstringLink>,
        diagnostics: Vec<Diagnostic>,
    },
    /// An instance file (markdown or pure-YAML) that declared a `type:`.
    /// `instance` is `None` when the structural parse failed.
    Instance {
        instance: Option<Instance>,
        /// The markdown body following the frontmatter; empty for pure-YAML.
        body: String,
        /// Byte offset of `body` within the source file.
        body_offset: usize,
        is_markdown: bool,
        /// Navigational links from the instance's `#:` docstrings (head,
        /// frontmatter fields, and nested records). A body-fence typed block's
        /// own docstring links are collected separately, not held here.
        doc_links: Vec<DocstringLink>,
        diagnostics: Vec<Diagnostic>,
    },
    /// A markdown file with frontmatter but no `type:` claim, or no
    /// frontmatter at all. A first-class graph citizen, held so the reference
    /// and content reads work for notes, not only typed instances. Not
    /// validated, so it carries no diagnostics of its own.
    Note {
        source_path: PathBuf,
        /// Top-level frontmatter fields; empty when the file has no
        /// frontmatter.
        fields: Vec<InstanceField>,
        /// The markdown body following the frontmatter; the whole file when
        /// there is no frontmatter.
        body: String,
        /// Byte offset of `body` within the source file.
        body_offset: usize,
        diagnostics: Vec<Diagnostic>,
    },
    /// Classified but not parsed into an instance. Holds any diagnostic the
    /// attempt produced (a bad encoding, an unterminated frontmatter, a YAML
    /// error).
    Unparsed { diagnostics: Vec<Diagnostic> },
}

impl FileParse {
    /// The parse-layer diagnostics this file produced.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        match self {
            FileParse::TypeDef { diagnostics, .. }
            | FileParse::Instance { diagnostics, .. }
            | FileParse::Note { diagnostics, .. }
            | FileParse::Unparsed { diagnostics } => diagnostics,
        }
    }

    /// The markdown body and its byte offset, for any file carrying one: a
    /// markdown instance or a note. `None` for pure-YAML instances, type-defs,
    /// and unparsed files.
    pub fn markdown_body(&self) -> Option<(&str, usize)> {
        match self {
            FileParse::Instance {
                body,
                body_offset,
                is_markdown: true,
                ..
            } => Some((body, *body_offset)),
            FileParse::Note {
                body, body_offset, ..
            } => Some((body, *body_offset)),
            _ => None,
        }
    }
}

/// The served kind of a catalogued file: `instance` / `type-def` / `note` /
/// `asset`.
///
/// ONE derivation behind every read that reports a file's kind (`files`,
/// `hubs`, a `neighborhood` node's `file_kind`), so the vocabulary cannot drift
/// per read.
///
/// `asset` is the distinction that matters. A catalogued file the build never
/// read has no parse, and folding that into `note` reports a PDF as prose:
/// wrong for `hubs`, where an asset carrying inbound `file*` references
/// genuinely ranks, and wrong for `files`, where the asset set is the whole
/// reason a consumer completing `[[` needs the read at all. See
/// [[spec - addressable enumeration reads - the plural of each resolve verb, one listing per wikilink fragment position]].
///
/// `None` (a path the catalog does not hold) reports `asset` for the same
/// reason: the engine has no parse, so it cannot claim the file is prose.
pub fn served_file_kind(parse: Option<&FileParse>) -> &'static str {
    match parse {
        Some(FileParse::TypeDef { .. }) => "type-def",
        Some(FileParse::Instance { .. }) => "instance",
        Some(FileParse::Note { .. }) => "note",
        // `Unparsed` is the asset arm: classified, catalogued, never read.
        Some(FileParse::Unparsed { .. }) | None => "asset",
    }
}

/// Parse one file's bytes into a [`FileParse`].
///
/// The path selects the parse strategy (type-def vs instance vs pure-YAML) and
/// attributes spans; the bytes are the content. Returns owned, source-free
/// data. A read failure is the caller's to handle, this runs only on bytes it
/// could read.
pub fn parse_file(path: &Path, bytes: &[u8]) -> FileParse {
    if classify_by_path(path) == Some(FileKind::TypeDef) {
        parse_type_def_file(path, bytes)
    } else if is_instance_candidate_path(path) {
        parse_instance_file(path, bytes)
    } else {
        FileParse::Unparsed {
            diagnostics: Vec::new(),
        }
    }
}

/// When a type-def fails YAML parse, detect the common unquoted-suffixed-enum
/// mistake and return an actionable fix.
///
/// A bare `myField: [a, b][]` is not valid YAML: `[a, b]` closes as a flow
/// sequence and the trailing `[]` / `[+]` has no valid parse, so the whole file
/// fails to load with a cryptic saphyr message ("did not find expected key").
/// The shape grammar accepts the string form directly, so quoting is the whole
/// fix. See [[type-def shape enum::au-type-system]].
///
/// The tell is a value that starts with `[` and contains `][]` or `][+]`, two
/// flow collections back to back, which valid unquoted YAML never produces. The
/// `value.starts_with('[')` guard keeps comment lines and already-quoted shapes
/// out. Advisory only, so a rare false match costs nothing.
fn suffixed_enum_quote_hint(source: &str) -> Option<SuggestedFix> {
    for line in source.lines() {
        let Some((lhs, rhs)) = line.split_once(':') else {
            continue;
        };
        let value = rhs.trim();
        if value.starts_with('[') && (value.contains("][]") || value.contains("][+]")) {
            let key = lhs.trim_start().trim_start_matches("- ").trim();
            return Some(SuggestedFix {
                description: format!(
                    "a suffixed inline enum must be quoted, its bare form is not valid YAML; write `{key}: \"{value}\"`"
                ),
            });
        }
    }
    None
}

fn parse_type_def_file(path: &Path, bytes: &[u8]) -> FileParse {
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => {
            let owned = String::from_utf8(bytes.to_vec()).unwrap_err();
            return FileParse::TypeDef {
                type_def: None,
                doc_links: Vec::new(),
                diagnostics: vec![not_utf8_diag(path, &owned)],
            };
        }
    };

    let mut diagnostics = Vec::new();
    let docs = match parse(text) {
        Ok(d) => d,
        Err(yaml_err) => {
            return FileParse::TypeDef {
                type_def: None,
                doc_links: Vec::new(),
                diagnostics: vec![Diagnostic {
                    code: YAML_PARSE_ERROR,
                    severity: Severity::Error,
                    span: Span::new(path.to_path_buf(), yaml_err.range),
                    message: yaml_err.message,
                    related: vec![],
                    fix: suffixed_enum_quote_hint(text),
                }],
            };
        }
    };
    // Install the codepoint→byte checkpoint index over the parsed slice (a
    // type-def is pure YAML, offset 0) BEFORE any span conversion, so
    // `span_to_byte_range` resolves each of the file's N spans in O(stride)
    // instead of a linear scan from the start — otherwise a large type-def is
    // O(N × length). The guard restores the prior index on drop.
    let _cp_index = au_parser::index_source(text);
    diagnostics.extend(duplicate_key_diags(path, text, text, 0));
    let Some(doc) = docs.first() else {
        return FileParse::TypeDef {
            type_def: None,
            doc_links: Vec::new(),
            diagnostics,
        };
    };
    let res = parse_type_def(path, text, 0, doc);
    diagnostics.extend(res.diagnostics);
    // A duplicate field in the `fields:` map surfaces as `duplicate-field`
    // (au-core, which knows the fields span). Drop the coinciding generic
    // `duplicate-key-in-mapping` from the scan above, so the fields case
    // carries one signal, not two, at the same span.
    //
    // The match is EXACT byte-range equality, which holds because both codes
    // anchor their primary span at the same `dup.duplicate_span` (au-core's
    // `duplicate-field` and this scan both use it). If either derivation ever
    // moves (e.g. to the key span vs the entry span), this would silently
    // fail into a double signal; the `duplicate-field` scenario snapshot is the
    // end-to-end guard that catches that regression.
    let dup_field_spans: Vec<ByteRange> = diagnostics
        .iter()
        .filter(|d| d.code.as_str() == "duplicate-field")
        .map(|d| d.span.range)
        .collect();
    if !dup_field_spans.is_empty() {
        diagnostics.retain(|d| {
            d.code.as_str() != "duplicate-key-in-mapping"
                || !dup_field_spans.contains(&d.span.range)
        });
    }
    FileParse::TypeDef {
        type_def: res.type_def,
        doc_links: res.doc_links,
        diagnostics,
    }
}

/// Whether the top-level YAML document writes a `type:` key. Drives the
/// `engine-schema-type-unwritten` drift: a stamped floor and a written
/// `type: au.engine.X::au-engine` yield the same claim, so the written-vs-stamped
/// distinction cannot be recovered from the claim alone, only from key presence.
fn has_written_type_key(doc: &MarkedYaml<'_>) -> bool {
    let YamlData::Mapping(m) = &doc.data else {
        return false;
    };
    m.iter()
        .any(|(key, _)| matches!(&key.data, YamlData::Value(Scalar::String(s)) if s == "type"))
}

/// Whether pure-YAML `content` writes a top-level `type:` key. For the config
/// channel's `type:` injection: a file that already self-describes is left as-is,
/// its own claim honored (and possibly a subtype of the declared floor). Malformed
/// YAML has no top-level key, so it reads as no claim and the floor is injected.
pub(crate) fn content_declares_type(content: &str) -> bool {
    matches!(parse(content), Ok(docs) if docs.first().is_some_and(has_written_type_key))
}

/// Parse an engine-schema file (`repo.yaml`, `.arsumbris/workspace.yaml`, the locks,
/// a device-config file) as a typed instance. The file KIND assigns the floor
/// `type_name` claim; a written `type:` is honored and wins, an absent one is
/// stamped with the floor and drifts (`engine-schema-type-unwritten`), so the
/// file self-describes on disk yet stays correct by kind either way. `type_name`
/// is the bare leaf (`au.engine.repo`). These are pure-YAML, so the whole file is
/// the frontmatter and there is no markdown body.
///
/// `owner` qualifies the stamped floor: `Some(repo)` writes `type_name::repo`,
/// the in-repo case where the file resolves as an importer of the builtin
/// `au-engine` peer. `None` writes a BARE `type_name`, for a device-global file
/// validated directly in the `au-engine` graph's own scope, where a `::au-engine`
/// self-qualifier would only add a redundant `type-repo-self` hint.
pub(crate) fn parse_engine_schema_instance(
    path: &Path,
    bytes: &[u8],
    type_name: &str,
    owner: Option<&str>,
) -> FileParse {
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => {
            let owned = String::from_utf8(bytes.to_vec()).unwrap_err();
            return FileParse::Unparsed {
                diagnostics: vec![not_utf8_diag(path, &owned)],
            };
        }
    };
    let docs = match parse(text) {
        Ok(d) => d,
        Err(yaml_err) => {
            return FileParse::Unparsed {
                diagnostics: vec![Diagnostic {
                    code: YAML_PARSE_ERROR,
                    severity: Severity::Error,
                    span: Span::new(path.to_path_buf(), yaml_err.range),
                    message: yaml_err.message,
                    related: vec![],
                    fix: None,
                }],
            };
        }
    };
    let _cp_index = au_parser::index_source(text);
    let mut diagnostics = duplicate_key_diags(path, text, text, 0);
    let empty_instance = |diagnostics| FileParse::Instance {
        instance: None,
        body: String::new(),
        body_offset: 0,
        is_markdown: false,
        doc_links: Vec::new(),
        diagnostics,
    };
    let Some(doc) = docs.first() else {
        return empty_instance(diagnostics);
    };
    // An engine-schema file self-describes with a written `type:` (the engine's
    // own writers emit the qualified `au.engine.X::au-engine`). An ABSENT written
    // type is `drift`: the kind still assigns the floor, so the file stays fully
    // correct, but it does not self-describe on disk. See
    // [[spec - engine-schema file claims - the kind assigns a floor, a written type self-describes and mixes in more]].
    if !has_written_type_key(doc) {
        // The self-describing form follows the file's validation scope: an
        // in-repo file (owner=Some) crosses to the builtin peer, so it is
        // `::au-engine`-qualified; a device file (owner=None) validates in the
        // builtin's own scope, so its claim is BARE (a self-qualifier would be a
        // redundant `type-repo-self`).
        let written_form = match owner {
            Some(repo) => format!("{type_name}::{repo}"),
            None => type_name.to_string(),
        };
        diagnostics.push(Diagnostic {
            code: crate::engine_schema::ENGINE_SCHEMA_TYPE_UNWRITTEN,
            severity: Severity::Drift,
            span: Span::new(path.to_path_buf(), ByteRange::new(0, 0)),
            message: format!(
                "engine-schema file carries no written `type:`; the kind assigns the floor `{written_form}` regardless, but the file does not self-describe on disk"
            ),
            related: vec![],
            fix: Some(SuggestedFix {
                description: format!("add `type: {written_form}` so the file self-describes"),
            }),
        });
    }
    let claim = match owner {
        Some(repo) => format!("{type_name}::{repo}"),
        None => type_name.to_string(),
    };
    let stamp = TypeNameClaim::parse(&claim, ByteRange::new(0, 0));
    let res = parse_instance_stamped(path, text, 0, doc, Some(stamp));
    diagnostics.extend(res.diagnostics);
    FileParse::Instance {
        instance: res.instance,
        body: String::new(),
        body_offset: 0,
        is_markdown: false,
        doc_links: res.doc_links,
        diagnostics,
    }
}

fn parse_instance_file(path: &Path, bytes: &[u8]) -> FileParse {
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => {
            let owned = String::from_utf8(bytes.to_vec()).unwrap_err();
            return FileParse::Unparsed {
                diagnostics: vec![not_utf8_diag(path, &owned)],
            };
        }
    };

    // Pure-YAML instances parse as a single document; routing them through
    // `split_frontmatter` would misread a leading `---` as a frontmatter
    // delimiter.
    let split = if is_pure_yaml_instance_path(path) {
        whole_as_frontmatter(text)
    } else {
        match split_frontmatter(text) {
            Ok(Some(s)) => s,
            Ok(None) => {
                // No frontmatter: a markdown note whose whole text is the body.
                return FileParse::Note {
                    source_path: path.to_path_buf(),
                    fields: Vec::new(),
                    body: text.to_string(),
                    body_offset: 0,
                    diagnostics: Vec::new(),
                };
            }
            Err(FrontmatterError::Unterminated { open_range }) => {
                return FileParse::Unparsed {
                    diagnostics: vec![Diagnostic {
                        code: FRONTMATTER_UNTERMINATED,
                        severity: Severity::Error,
                        span: Span::new(path.to_path_buf(), open_range),
                        message: "frontmatter opens with '---' but never closes — the YAML body is unrecoverable until a closing '---' line is added".to_string(),
                        related: vec![],
                        fix: None,
                    }],
                };
            }
        }
    };

    let frontmatter_offset = split.frontmatter_range.start;
    let docs = match parse(split.frontmatter) {
        Ok(d) => d,
        Err(yaml_err) => {
            return FileParse::Unparsed {
                diagnostics: vec![Diagnostic {
                    code: YAML_PARSE_ERROR,
                    severity: Severity::Error,
                    span: Span::new(
                        path.to_path_buf(),
                        ByteRange::new(
                            yaml_err.range.start + frontmatter_offset,
                            yaml_err.range.end + frontmatter_offset,
                        ),
                    ),
                    message: yaml_err.message,
                    related: vec![],
                    fix: None,
                }],
            };
        }
    };

    let Some(doc) = docs.first() else {
        return FileParse::Unparsed {
            diagnostics: Vec::new(),
        };
    };

    // Install the codepoint→byte checkpoint index over the SAME slice
    // `span_to_byte_range` converts against (`&text[frontmatter_offset..]`, the
    // YAML body plus any markdown tail) BEFORE the span-conversion storm below.
    // The index keys on the slice's pointer and length, so it must be this exact
    // slice or the conversions silently keep the O(N × length) linear fallback —
    // the difference between a large session-log parsing in milliseconds versus
    // pegging a CPU for minutes. The guard restores the prior index on drop.
    let _cp_index = au_parser::index_source(&text[frontmatter_offset..]);

    // No top-level `type:` — a note, not an instance. Pure-YAML files are data
    // instances, so only markdown without a type claim becomes a note; a
    // pure-YAML file with no `type:` stays unparsed.
    if !frontmatter_has_type(doc) {
        if is_pure_yaml_instance_path(path) {
            return FileParse::Unparsed {
                diagnostics: Vec::new(),
            };
        }
        let note = parse_note(path, text, frontmatter_offset, doc);
        return FileParse::Note {
            source_path: note.source_path,
            fields: note.fields,
            body: split.body.to_string(),
            body_offset: split.body_range.start,
            diagnostics: Vec::new(),
        };
    }

    let mut diagnostics = duplicate_key_diags(path, text, split.frontmatter, frontmatter_offset);
    let parsed = parse_instance(path, text, frontmatter_offset, doc);
    diagnostics.extend(parsed.diagnostics);

    let is_markdown = !is_pure_yaml_instance_path(path);
    let (body, body_offset) = if is_markdown {
        (split.body.to_string(), split.body_range.start)
    } else {
        (String::new(), text.len())
    };

    FileParse::Instance {
        instance: parsed.instance,
        body,
        body_offset,
        is_markdown,
        doc_links: parsed.doc_links,
        diagnostics,
    }
}

/// True when a parsed frontmatter document declares a top-level `type:` key.
fn frontmatter_has_type(doc: &MarkedYaml<'_>) -> bool {
    let YamlData::Mapping(m) = &doc.data else {
        return false;
    };
    m.iter()
        .any(|(k, _)| matches!(&k.data, YamlData::Value(Scalar::String(s)) if s.as_ref() == "type"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parses_a_markdown_instance_with_body() {
        let path = PathBuf::from("note.md");
        let src = b"---\ntype: thing\ntitle: hello\n---\n# Section\nbody text\n";
        let parse = parse_file(&path, src);
        match parse {
            FileParse::Instance {
                instance,
                body,
                is_markdown,
                diagnostics,
                ..
            } => {
                assert!(instance.is_some(), "instance should parse");
                assert!(is_markdown);
                assert!(body.contains("# Section"), "body captured");
                assert!(diagnostics.is_empty(), "clean instance has no diagnostics");
            }
            other => panic!("expected Instance, got {other:?}"),
        }
    }

    #[test]
    fn suffixed_enum_yaml_error_carries_a_quote_hint() {
        // `[a, b, c][]` is not valid YAML (flow sequence then a stray `[]`), so
        // the file fails to load with a cryptic saphyr message. The parse error
        // must carry an actionable fix telling the author to quote the shape.
        let path = PathBuf::from("type/thing.type.yaml");
        let src = b"fields:\n  myField: [a, b, c][]\n";
        match parse_file(&path, src) {
            FileParse::TypeDef {
                type_def,
                diagnostics,
                ..
            } => {
                assert!(type_def.is_none(), "the file fails YAML parse");
                let d = &diagnostics[0];
                assert_eq!(d.code.as_str(), "yaml-parse-error");
                let fix = d.fix.as_ref().expect("a quote hint is attached");
                assert!(
                    fix.description.contains("must be quoted"),
                    "fix names the quoting rule: {}",
                    fix.description
                );
                assert!(
                    fix.description.contains("\"[a, b, c][]\""),
                    "fix shows the quoted shape: {}",
                    fix.description
                );
            }
            other => panic!("expected TypeDef, got {other:?}"),
        }
    }

    #[test]
    fn an_unrelated_type_def_yaml_error_has_no_quote_hint() {
        // A YAML error with no suffixed-enum pattern must not carry the hint.
        let path = PathBuf::from("type/thing.type.yaml");
        let src = b"fields: [\n";
        match parse_file(&path, src) {
            FileParse::TypeDef { diagnostics, .. } => {
                let d = &diagnostics[0];
                assert_eq!(d.code.as_str(), "yaml-parse-error");
                assert!(d.fix.is_none(), "no spurious hint on an unrelated error");
            }
            other => panic!("expected TypeDef, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_type_def() {
        let path = PathBuf::from("type/thing.type.yaml");
        let src = b"extends: thing\nfields:\n  title: String\n";
        match parse_file(&path, src) {
            FileParse::TypeDef { type_def, .. } => {
                assert!(type_def.is_some(), "type-def should parse");
            }
            other => panic!("expected TypeDef, got {other:?}"),
        }
    }

    #[test]
    fn a_parse_installs_the_codepoint_index_so_spans_skip_the_linear_fallback() {
        // A session-log-shaped instance: many nested records, each field and value
        // carrying a span that `span_to_byte_range` converts. `parse_file` must
        // install the checkpoint index over the file's body, so every conversion is
        // O(stride) and takes ZERO linear-scan fallbacks. Without the index each
        // span rescans from the start — the O(N × length) daemon-peg a large
        // session-log reparse hits. Pure YAML (offset 0), so the convert slice
        // and the indexed slice coincide.
        let mut src = String::from("type: session-log\nsession: s1\nevents:\n");
        for i in 0..200 {
            src.push_str(&format!(
                "  - type: event\n    at: \"t{i}\"\n    note: \"line {i}\"\n"
            ));
        }
        let path = PathBuf::from("s.yaml");
        let _ = au_parser::take_cp_fallback_count(); // clear any prior count
        let parse = parse_file(&path, src.as_bytes());
        assert!(
            matches!(
                parse,
                FileParse::Instance {
                    instance: Some(_),
                    ..
                }
            ),
            "the fixture parses as an instance"
        );
        assert_eq!(
            au_parser::take_cp_fallback_count(),
            0,
            "every span converted through the installed index; a nonzero count means the O(N^2) linear fallback was taken (the index was not installed for the parsed slice)"
        );
    }

    #[test]
    fn a_type_def_parse_installs_the_codepoint_index_so_spans_skip_the_linear_fallback() {
        // The type-def sibling of the instance test above. A large `.type.yaml`
        // (many field declarations, each key and shape carrying a span that
        // `span_to_byte_range` converts) must convert every span through the
        // index `parse_file` installs at the type-def branch — zero linear-scan
        // fallbacks. This is the exact path an alpha tester flagged as regressing
        // ("reproduced from a type-def write"): the instance path was wired first,
        // the type-def path (pure YAML, offset 0) rides the same guard.
        let mut src = String::from("extends: thing\nfields:\n");
        for i in 0..200 {
            src.push_str(&format!("  - field{i}: String\n"));
        }
        let path = PathBuf::from("type/thing.type.yaml");
        let _ = au_parser::take_cp_fallback_count(); // clear any prior count
        let parse = parse_file(&path, src.as_bytes());
        assert!(
            matches!(
                parse,
                FileParse::TypeDef {
                    type_def: Some(_),
                    ..
                }
            ),
            "the fixture parses as a type-def"
        );
        assert_eq!(
            au_parser::take_cp_fallback_count(),
            0,
            "every type-def span converted through the installed index; a nonzero count means the O(N^2) linear fallback was taken (the index was not installed for the parsed slice)"
        );
    }

    #[test]
    fn a_markdown_note_without_type_captures_frontmatter_and_body() {
        let path = PathBuf::from("note.md");
        let src = b"---\ntitle: just a note\n---\nbody\n";
        match parse_file(&path, src) {
            FileParse::Note {
                fields,
                body,
                diagnostics,
                ..
            } => {
                assert!(diagnostics.is_empty(), "notes are not validated");
                assert_eq!(fields.len(), 1, "frontmatter field captured");
                assert_eq!(fields[0].key, "title");
                assert!(body.contains("body"), "body captured");
            }
            other => panic!("expected Note, got {other:?}"),
        }
    }

    #[test]
    fn a_markdown_note_without_frontmatter_is_all_body() {
        let path = PathBuf::from("plain.md");
        let src = b"# Just prose\n\nSee [[other]].\n";
        match parse_file(&path, src) {
            FileParse::Note {
                fields,
                body,
                body_offset,
                ..
            } => {
                assert!(fields.is_empty(), "no frontmatter, no fields");
                assert_eq!(body_offset, 0, "the whole file is the body");
                assert!(body.contains("[[other]]"));
            }
            other => panic!("expected Note, got {other:?}"),
        }
    }

    #[test]
    fn a_pure_yaml_file_without_type_stays_unparsed() {
        // Pure-YAML files are data instances, not notes; without a `type:`
        // claim they stay unparsed, not captured as a note.
        let path = PathBuf::from("data.yaml");
        let src = b"title: just data\n";
        match parse_file(&path, src) {
            FileParse::Unparsed { diagnostics } => assert!(diagnostics.is_empty()),
            other => panic!("expected Unparsed, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_frontmatter_keys_are_diagnosed() {
        let path = PathBuf::from("note.md");
        let src = b"---\ntype: thing\ntitle: a\ntitle: b\n---\nbody\n";
        let parse = parse_file(&path, src);
        assert!(
            parse
                .diagnostics()
                .iter()
                .any(|d| d.code.as_str() == "duplicate-key-in-mapping"),
            "duplicate key surfaces in the single parse"
        );
    }
}
