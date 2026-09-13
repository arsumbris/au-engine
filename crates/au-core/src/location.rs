//! Type-level location constraints: the parsed `location:` block on a
//! [[type-def::au-type-system]], and the pure string parsers for its `name` template and
//! `path` glob.
//!
//! A [`LocationSpec`] constrains where a type's instances live. It is ADVISORY
//! (a mismatch is a diagnostic, never a block) and it NEVER participates in
//! type identity, so it is held on `TypeDef` beside `doc` and excluded from the
//! canonical hash. The per-instance matching and the field-safety load checks
//! live in `validate` / `load_checks`; this module owns the block's AST and the
//! two mini-grammars. See
//! [[spec - location constraints - a name template and path predicate as an advisory placement meet]].

use au_diagnostics::ByteRange;

/// The parsed `location:` block. Every sub-key is optional; an absent block is
/// `None` on the `TypeDef`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocationSpec {
    /// The basename-stem template, a pure function of the file's own fields.
    pub name: Option<NameTemplate>,
    /// The repo-relative directory glob.
    pub path: Option<PathGlob>,
    /// The file-extension pin, `md` or `yaml`.
    pub file_type: Option<FileType>,
    /// `strict: true` makes the location mandatory: a mismatch is an error and
    /// it opts out of the mixin satisfy-any.
    pub strict: bool,
    /// The `location:` value span, the fallback anchor for a load-check
    /// diagnostic when a more specific sub-key span is absent.
    pub block_span: ByteRange,
    /// The `name` value span, anchors the name-field-safety load check.
    pub name_span: ByteRange,
    /// The `fileType` value span, anchors the fileType/body-conflict load check.
    pub file_type_span: ByteRange,
}

/// A `name` template: a sequence of literal and substitution segments that
/// render the file's basename STEM. Pure over the file's own fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameTemplate {
    pub segments: Vec<NameSegment>,
    /// The raw template string, for diagnostics, round-trip, and the wire.
    pub raw: String,
}

/// One segment of a [`NameTemplate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameSegment {
    /// Literal text between substitutions.
    Literal(String),
    /// `${.type}`, the owning type's name.
    Type,
    /// `${.field}`, a field value rendered as its canonical string.
    Field(String),
}

/// A static glob over the repo-relative directory a file must sit in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathGlob {
    pub segments: Vec<GlobSegment>,
    /// Whether the source carried a trailing `/`. Informational for now; the
    /// matcher treats the glob as a directory the file must sit inside.
    pub trailing_slash: bool,
    /// The raw glob string, for diagnostics, round-trip, and the wire.
    pub raw: String,
}

/// One segment of a [`PathGlob`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobSegment {
    /// A literal path segment.
    Literal(String),
    /// `*`, exactly one segment.
    Star,
    /// `**`, zero or more segments.
    DoubleStar,
}

/// The file-extension pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Md,
    Yaml,
}

impl LocationSpec {
    /// Structural equality of the CONSTRAINT, `name` / `path` / `fileType` /
    /// `strict`, ignoring the diagnostic spans. Two claims with token-equal
    /// blocks auto-unify under this, see the mixin resolution.
    pub fn constraint_eq(&self, other: &LocationSpec) -> bool {
        self.name == other.name
            && self.path == other.path
            && self.file_type == other.file_type
            && self.strict == other.strict
    }
}

/// A legal field / type name: a leading letter, then letters, digits, `_`, `-`.
/// The `name` template's `${.field}` reference is checked against this shape;
/// whether the field EXISTS and renders safely is a `load_checks` concern.
fn is_legal_field_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl NameTemplate {
    /// Parse a `name` template string into segments. `Err` carries a message
    /// the caller surfaces as `location-bad-shape`.
    pub fn parse(raw: &str) -> Result<NameTemplate, String> {
        let mut segments = Vec::new();
        let mut lit = String::new();
        let mut rest = raw;
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix("${") {
                if !lit.is_empty() {
                    segments.push(NameSegment::Literal(std::mem::take(&mut lit)));
                }
                let end = after
                    .find('}')
                    .ok_or_else(|| "unterminated `${` in name template".to_string())?;
                segments.push(parse_token(&after[..end])?);
                rest = &after[end + 1..];
            } else {
                let ch = rest.chars().next().unwrap();
                lit.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
        if !lit.is_empty() {
            segments.push(NameSegment::Literal(lit));
        }
        if segments.is_empty() {
            return Err("name template is empty".to_string());
        }
        Ok(NameTemplate {
            segments,
            raw: raw.to_string(),
        })
    }
}

/// Parse the text between `${` and `}` into a substitution segment.
fn parse_token(inner: &str) -> Result<NameSegment, String> {
    let field = inner.strip_prefix('.').ok_or_else(|| {
        format!("`${{{inner}}}` must reference a field, write `${{.field}}` or `${{.type}}`")
    })?;
    if field == "type" {
        return Ok(NameSegment::Type);
    }
    if field.is_empty() {
        return Err("empty field reference `${.}`".to_string());
    }
    if field.contains('.') {
        return Err(format!(
            "nested field access `${{.{field}}}` is not supported"
        ));
    }
    if !is_legal_field_name(field) {
        return Err(format!("`{field}` is not a legal field name"));
    }
    Ok(NameSegment::Field(field.to_string()))
}

impl PathGlob {
    /// Parse a `path` glob string. Rooted at the repo root, `*` matches one
    /// segment, `**` any depth. Absolute, `..`, `.`, an empty segment, or a
    /// partial-glob segment (`foo*`) each `Err`.
    pub fn parse(raw: &str) -> Result<PathGlob, String> {
        if raw.starts_with('/') {
            return Err("location path must be repo-relative, not absolute".to_string());
        }
        let trailing_slash = raw.ends_with('/');
        let trimmed = raw.strip_suffix('/').unwrap_or(raw);
        let mut segments = Vec::new();
        if !trimmed.is_empty() {
            for seg in trimmed.split('/') {
                match seg {
                    "" => return Err("location path has an empty segment (`//`)".to_string()),
                    ".." => return Err("`..` is not allowed in a location path".to_string()),
                    "." => return Err("`.` is redundant in a location path".to_string()),
                    "*" => segments.push(GlobSegment::Star),
                    "**" => segments.push(GlobSegment::DoubleStar),
                    s if s.contains('*') => {
                        return Err(format!(
                            "`{s}`: `*` and `**` must each be a whole path segment"
                        ));
                    }
                    s => segments.push(GlobSegment::Literal(s.to_string())),
                }
            }
        }
        Ok(PathGlob {
            segments,
            trailing_slash,
            raw: raw.to_string(),
        })
    }
}

impl FileType {
    /// Parse the `fileType` pin. `None` for anything but `md` / `yaml`.
    pub fn parse(s: &str) -> Option<FileType> {
        match s {
            "md" => Some(FileType::Md),
            "yaml" => Some(FileType::Yaml),
            _ => None,
        }
    }

    /// The file extension this pin matches, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            FileType::Md => "md",
            FileType::Yaml => "yaml",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_template_literal_and_subs() {
        let t = NameTemplate::parse("${.type} - ${.createdAt} - ${.slug}").unwrap();
        assert_eq!(
            t.segments,
            vec![
                NameSegment::Type,
                NameSegment::Literal(" - ".into()),
                NameSegment::Field("createdAt".into()),
                NameSegment::Literal(" - ".into()),
                NameSegment::Field("slug".into()),
            ]
        );
    }

    #[test]
    fn name_template_bare_literal() {
        let t = NameTemplate::parse("governance").unwrap();
        assert_eq!(t.segments, vec![NameSegment::Literal("governance".into())]);
    }

    #[test]
    fn name_template_errors() {
        assert!(NameTemplate::parse("${.slug").is_err()); // unterminated
        assert!(NameTemplate::parse("${.}").is_err()); // empty field
        assert!(NameTemplate::parse("${slug}").is_err()); // missing dot
        assert!(NameTemplate::parse("${.a.b}").is_err()); // nested access
        assert!(NameTemplate::parse("${.1bad}").is_err()); // illegal field name
        assert!(NameTemplate::parse("").is_err()); // empty template
    }

    #[test]
    fn path_glob_segments() {
        let g = PathGlob::parse("**/plan/").unwrap();
        assert_eq!(
            g.segments,
            vec![GlobSegment::DoubleStar, GlobSegment::Literal("plan".into())]
        );
        assert!(g.trailing_slash);

        let g2 = PathGlob::parse("config/*").unwrap();
        assert_eq!(
            g2.segments,
            vec![GlobSegment::Literal("config".into()), GlobSegment::Star]
        );
        assert!(!g2.trailing_slash);
    }

    #[test]
    fn path_glob_root_is_empty() {
        let g = PathGlob::parse("").unwrap();
        assert!(g.segments.is_empty());
    }

    #[test]
    fn path_glob_errors() {
        assert!(PathGlob::parse("/etc/x").is_err()); // absolute
        assert!(PathGlob::parse("../peer").is_err()); // parent escape
        assert!(PathGlob::parse("a/./b").is_err()); // redundant dot
        assert!(PathGlob::parse("a//b").is_err()); // empty segment
        assert!(PathGlob::parse("foo*bar").is_err()); // partial glob
    }

    #[test]
    fn file_type_parse() {
        assert_eq!(FileType::parse("md"), Some(FileType::Md));
        assert_eq!(FileType::parse("yaml"), Some(FileType::Yaml));
        assert_eq!(FileType::parse("yml"), None);
        assert_eq!(FileType::parse("txt"), None);
    }
}
