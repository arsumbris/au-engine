//! The type-name reference index: which sources reference a type-def by name.
//!
//! Where [`crate::refnames`] inverts the names WIKILINKS target, this inverts
//! the names TYPE-NAME references mention: `type:` claims (parent and identity),
//! `sealed:` branches, slot shapes (`name*`, `type<name>*`, `<a | b>`),
//! qualifier keys (`field{name}`), body `use: name`, meta `- type: name`, and
//! nested inline-record `type:` claims. None of these is a wikilink, so the
//! wikilink machinery never sees them.
//!
//! It answers "who references type X" ([`KnowledgeBase::type_referrers`](crate::ir::KnowledgeBase::type_referrers))
//! and drives the `rename_type` cascade ([`type_ref_edits`]). The query is
//! computed on demand over the always-current catalog, so there is no standing
//! cache to keep in incremental parity; a materialised index is a later
//! optimisation if a high-frequency consumer pulls it.
//!
//! Repo-local: a type-name resolves within its own repo (global-by-name), so a
//! reference in another repo names that repo's own copy, never this one. The
//! caller scopes the scan to the owning repo. The cross-repo surface a type-def
//! rename also touches — `[[name::repo]]` wikilinks to the def file — is the
//! wikilink backlink index's job, not this one.

use au_core::{
    BodyItem, InlineValue, Instance, InstanceField, InstanceValue, TypeClaim, TypeDef, TypeName,
};
use au_diagnostics::ByteRange;

use crate::parse::FileParse;

/// One type-name reference site in a parsed file.
///
/// A site's `repo` is its `::repo` qualifier, `None` for a bare name that names
/// the source file's OWN repo. The qualifier is a global repo NAME (resolved by
/// [`crate::ir::KnowledgeBase`] via `repos.by_name`), never a per-file alias, so a match
/// is a name comparison, `site.repo.unwrap_or(file_repo) == def_repo`.
enum Site<'a> {
    /// A whole type-name token at `span`: a `type:` / `sealed:` / `use:` / meta
    /// `type:` name, or an inline-record `type:` claim. The span covers the whole
    /// authored form (`name` or `name::repo`), so the rewrite replaces it outright
    /// with the new name plus the same qualifier.
    Name {
        name: &'a str,
        repo: Option<&'a str>,
        span: ByteRange,
    },
    /// A qualified instance key `field{T}` or `field{T::repo}`. `qualifier` is
    /// the type-name `T` inside the braces, `span` covers just `T` (the `field{`,
    /// `::repo`, and `}` bytes around it are untouched by the rewrite).
    Qualifier {
        qualifier: &'a str,
        repo: Option<&'a str>,
        span: ByteRange,
    },
    /// A field slot shape. `raw` is the whole shape expression at `span`; a
    /// type-name lives inside it as a token (`name*`, `name::repo*`,
    /// `type<name>*`, `<name | other>`), each carrying its own optional `::repo`,
    /// so the rewrite re-tokenises rather than replacing the whole span blindly.
    Shape { raw: &'a str, span: ByteRange },
}

/// Walk every type-name reference site in `parse`, calling `f` on each.
fn for_each_site<'a>(parse: &'a FileParse, mut f: impl FnMut(Site<'a>)) {
    match parse {
        FileParse::TypeDef {
            type_def: Some(td), ..
        } => visit_type_def(td, &mut f),
        FileParse::Instance {
            instance: Some(inst),
            ..
        } => visit_instance(inst, &mut f),
        // A note carries no `type:` claim, but its field values may hold inline
        // records or qualified keys, so the same field walk applies.
        FileParse::Note { fields, .. } => {
            for field in fields {
                visit_field(field, &mut f);
            }
        }
        _ => {}
    }
}

fn visit_type_def<'a>(td: &'a TypeDef, f: &mut impl FnMut(Site<'a>)) {
    for c in &td.parents {
        f(Site::Name {
            name: c.name.as_str(),
            repo: c.repo.as_deref(),
            span: c.span,
        });
    }
    for c in &td.sealed {
        f(Site::Name {
            name: c.name.as_str(),
            repo: c.repo.as_deref(),
            span: c.span,
        });
    }
    for fd in &td.fields {
        f(Site::Shape {
            raw: &fd.raw_shape,
            span: fd.shape_span,
        });
    }
    if let Some(metas) = &td.meta_blocks {
        for mb in metas {
            f(Site::Name {
                name: mb.type_name.as_str(),
                repo: mb.repo.as_deref(),
                span: mb.type_name_span,
            });
            for field in &mb.fields {
                visit_field(field, f);
            }
        }
    }
    if let Some(body) = &td.body {
        visit_body_items(body, f);
    }
}

fn visit_body_items<'a>(items: &'a [BodyItem], f: &mut impl FnMut(Site<'a>)) {
    for item in items {
        match item {
            BodyItem::Use {
                type_name,
                repo,
                type_name_span,
                ..
            } => f(Site::Name {
                name: type_name.as_str(),
                repo: repo.as_deref(),
                span: *type_name_span,
            }),
            BodyItem::Section {
                body: Some(sub), ..
            } => visit_body_items(sub, f),
            _ => {}
        }
    }
}

fn visit_instance<'a>(inst: &'a Instance, f: &mut impl FnMut(Site<'a>)) {
    visit_type_claim(&inst.type_claim, f);
    for field in &inst.fields {
        visit_field(field, f);
    }
}

fn visit_type_claim<'a>(claim: &'a TypeClaim, f: &mut impl FnMut(Site<'a>)) {
    let items = match claim {
        TypeClaim::Bare(c) => std::slice::from_ref(c),
        TypeClaim::List { items, .. } => items.as_slice(),
    };
    for c in items {
        f(Site::Name {
            name: c.name.as_str(),
            repo: c.repo.as_deref(),
            span: c.span,
        });
    }
}

fn visit_field<'a>(field: &'a InstanceField, f: &mut impl FnMut(Site<'a>)) {
    // A qualified key `field{T}` (or `field{T::repo}`) names a type-name T inside
    // the braces. A `^:` block-id key is identity-layer, never a qualifier.
    if !field.key.starts_with('^') {
        if let Some(open) = field.key.find('{') {
            // The qualifier type runs from just after `{` to the first `::` or `}`.
            let after = &field.key[open + 1..];
            let close = after.find('}').unwrap_or(after.len());
            let sep = after.find("::").unwrap_or(after.len());
            let end = close.min(sep);
            let qualifier = &after[..end];
            if !qualifier.is_empty() {
                // An optional `::repo` sits between the type-name and the `}`.
                let repo = (sep < close).then(|| &after[sep + 2..close]);
                let name_start = field.key_span.start + open + 1;
                f(Site::Qualifier {
                    qualifier,
                    repo,
                    span: ByteRange::new(name_start, name_start + qualifier.len()),
                });
            }
        }
    }
    visit_value(&field.value, f);
}

fn visit_value<'a>(value: &'a InstanceValue, f: &mut impl FnMut(Site<'a>)) {
    match value {
        InstanceValue::Mapping(InlineValue {
            type_claim, fields, ..
        }) => {
            if let Some(tc) = type_claim {
                visit_type_claim(tc, f);
            }
            for field in fields {
                visit_field(field, f);
            }
        }
        InstanceValue::Sequence(elems) => {
            for e in elems {
                visit_value(&e.value, f);
            }
        }
        _ => {}
    }
}

/// Whether a site's `::repo` qualifier resolves to `def_repo`. A bare site
/// (`None`) names the source file's own repo, a qualified one names the repo it
/// spells. Both compare by repo NAME, the qualifier is not aliased.
fn site_targets(site_repo: Option<&str>, file_repo: &str, def_repo: &str) -> bool {
    site_repo.unwrap_or(file_repo) == def_repo
}

/// Whether `parse` (a file in `file_repo`) references the def `(def_name,
/// def_repo)` at any type-name reference site. The "who references type X"
/// predicate, repo-aware: a bare `foo` matches only when `file_repo == def_repo`,
/// a `foo::def_repo` matches from any repo, and a `foo::other` never does.
pub fn references_type(
    parse: &FileParse,
    file_repo: &str,
    def_name: &TypeName,
    def_repo: &str,
) -> bool {
    let target = def_name.as_str();
    let mut found = false;
    for_each_site(parse, |site| {
        if found {
            return;
        }
        found = match site {
            Site::Name { name, repo, .. } => {
                name == target && site_targets(repo, file_repo, def_repo)
            }
            Site::Qualifier {
                qualifier, repo, ..
            } => qualifier == target && site_targets(repo, file_repo, def_repo),
            Site::Shape { raw, .. } => qualified_token_positions(raw, target)
                .any(|(_, repo)| site_targets(repo, file_repo, def_repo)),
        };
    });
    found
}

/// The byte edits that rewrite every reference to the def `(old, def_repo)` from
/// a file in `file_repo` into `new`, QUALIFIER-PRESERVING. An own-repo reference
/// (bare `old`) becomes bare `new`; a mounted cross-repo reference (`old::def_repo`)
/// becomes `new::def_repo`, the qualifier survives. Computed from the parse alone
/// (a shape carries its `raw_shape`, a claim its authored span); the spans are
/// valid only while the content is unchanged since the parse, which the caller
/// guards by hash before applying.
pub fn type_ref_edits(
    parse: &FileParse,
    file_repo: &str,
    old: &TypeName,
    def_repo: &str,
    new: &TypeName,
) -> Vec<(ByteRange, String)> {
    let old = old.as_str();
    let new = new.as_str();
    let mut edits: Vec<(ByteRange, String)> = Vec::new();
    for_each_site(parse, |site| match site {
        Site::Name { name, repo, span } => {
            if name == old && site_targets(repo, file_repo, def_repo) {
                // The span covers the whole authored form (`foo` or `foo::repoA`),
                // so re-emit the new name with the SAME qualifier the site had.
                let replacement = match repo {
                    Some(r) => format!("{new}::{r}"),
                    None => new.to_string(),
                };
                edits.push((span, replacement));
            }
        }
        Site::Qualifier {
            qualifier,
            repo,
            span,
        } => {
            if qualifier == old && site_targets(repo, file_repo, def_repo) {
                // The span covers only the type-name head; the `::repo` and
                // `:field` bytes after it are left untouched.
                edits.push((span, new.to_string()));
            }
        }
        Site::Shape { raw, span } => {
            if let Some(rewritten) = rewrite_shape_tokens(raw, old, new, file_repo, def_repo) {
                edits.push((span, rewritten));
            }
        }
    });
    edits
}

/// A type-name continuation byte: letters, digits, `_`, `-`, and the internal
/// `.` of a sealed leaf. So `mcp.tool` is one token, never a prefix of
/// `mcp.tool.propose`.
fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.'
}

/// The byte offsets in `raw` where `needle` occurs as a whole type-name token,
/// bounded by a non-name byte (a shape operator or whitespace) or a string end.
/// Type names are ASCII, so byte indexing is exact.
fn token_positions<'a>(raw: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    let bytes = raw.as_bytes();
    let nlen = needle.len().max(1);
    let mut from = 0usize;
    std::iter::from_fn(move || {
        while let Some(rel) = raw.get(from..).and_then(|s| s.find(needle)) {
            let start = from + rel;
            from = start + nlen;
            let before_ok = start == 0 || !is_name_byte(bytes[start - 1]);
            let after = start + needle.len();
            let after_ok = after >= raw.len() || !is_name_byte(bytes[after]);
            if before_ok && after_ok {
                return Some(start);
            }
        }
        None
    })
}

/// Whole-token occurrences of `needle` in `raw`, each with its `::repo` suffix
/// (`None` for a bare token). A shape type-name carries the same optional `::repo`
/// a claim does (`foo::repoA*`), immediately after the name and before any shape
/// operator, so the qualifier is the name bytes right after `::`.
fn qualified_token_positions<'a>(
    raw: &'a str,
    needle: &'a str,
) -> impl Iterator<Item = (usize, Option<&'a str>)> + 'a {
    let bytes = raw.as_bytes();
    token_positions(raw, needle).map(move |start| {
        let after = start + needle.len();
        let repo = if raw[after..].starts_with("::") {
            let rstart = after + 2;
            let mut rend = rstart;
            while rend < bytes.len() && is_name_byte(bytes[rend]) {
                rend += 1;
            }
            (rend > rstart).then(|| &raw[rstart..rend])
        } else {
            None
        };
        (start, repo)
    })
}

/// Rewrite every whole-token occurrence of `old` in the shape `raw` that targets
/// the def `(old, def_repo)` from a file in `file_repo` to `new`, QUALIFIER-
/// PRESERVING: only the `old` name bytes are replaced, any `::repo` suffix and
/// every operator stay. `None` when no token targets the def (e.g. a `foo::other`
/// slot when renaming `foo` in `def_repo`), so the caller emits no edit.
fn rewrite_shape_tokens(
    raw: &str,
    old: &str,
    new: &str,
    file_repo: &str,
    def_repo: &str,
) -> Option<String> {
    let positions: Vec<usize> = qualified_token_positions(raw, old)
        .filter(|(_, repo)| site_targets(*repo, file_repo, def_repo))
        .map(|(start, _)| start)
        .collect();
    if positions.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(raw.len());
    let mut last = 0;
    for start in positions {
        out.push_str(&raw[last..start]);
        out.push_str(new);
        last = start + old.len();
    }
    out.push_str(&raw[last..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rewrite a shape's own-repo `old` tokens (file and def in the same repo
    /// `r`), returning the shape unchanged when nothing matches.
    fn rw(raw: &str, old: &str, new: &str) -> String {
        rewrite_shape_tokens(raw, old, new, "r", "r").unwrap_or_else(|| raw.to_string())
    }

    #[test]
    fn replaces_whole_tokens_across_shape_operators() {
        // The name sits behind every shape operator form; each whole-token
        // occurrence is rewritten, the operators untouched.
        assert_eq!(rw("foo*", "foo", "bar"), "bar*");
        assert_eq!(rw("foo&", "foo", "bar"), "bar&");
        assert_eq!(rw("foo*[]", "foo", "bar"), "bar*[]");
        assert_eq!(rw("type<foo>*", "foo", "bar"), "type<bar>*");
        assert_eq!(rw("<foo | other>", "foo", "bar"), "<bar | other>");
        assert_eq!(rw("<a | foo>[+]", "foo", "bar"), "<a | bar>[+]");
        // Pinned and def-ref-with-bound forms.
        assert_eq!(rw("foo*@", "foo", "bar"), "bar*@");
    }

    #[test]
    fn never_matches_a_dotted_supertoken() {
        // `mcp.tool` must not match inside `mcp.tool.propose` — the `.` is a
        // name byte, so the longer dotted name is one token.
        assert_eq!(
            rw("type<mcp.tool.propose>*", "mcp.tool", "x"),
            "type<mcp.tool.propose>*"
        );
        // But it does match the exact dotted leaf.
        assert_eq!(rw("mcp.tool.propose*", "mcp.tool.propose", "x"), "x*");
        // And a different sibling under the same parent is left alone.
        assert_eq!(
            rw("<mcp.tool | mcp.resource>", "mcp.tool", "mcp.device"),
            "<mcp.device | mcp.resource>"
        );
    }

    #[test]
    fn leaves_unrelated_and_builtin_names_untouched() {
        assert_eq!(rw("file*", "foo", "bar"), "file*");
        assert_eq!(rw("String", "foo", "bar"), "String");
        assert_eq!(rw("<file* | any*>", "foo", "bar"), "<file* | any*>");
        // A name that merely contains the needle as a substring is not a token.
        assert_eq!(rw("foobar*", "foo", "x"), "foobar*");
        assert_eq!(rw("prefoo*", "foo", "x"), "prefoo*");
    }

    #[test]
    fn a_qualified_shape_token_rewrites_preserving_its_repo() {
        // Renaming `foo` in repo `base`: a `foo::base*` slot (from any repo)
        // becomes `bar::base*`, the `::base` survives.
        assert_eq!(
            rewrite_shape_tokens("foo::base*", "foo", "bar", "app", "base"),
            Some("bar::base*".to_string())
        );
        // Inside a compound too.
        assert_eq!(
            rewrite_shape_tokens("<foo::base | other>", "foo", "bar", "app", "base"),
            Some("<bar::base | other>".to_string())
        );
    }

    #[test]
    fn a_shape_token_for_a_different_repo_is_not_rewritten() {
        // Renaming `foo` in `base` must not touch `foo::other` (other's foo) or the
        // consumer's own bare `foo` (app's own foo).
        assert_eq!(
            rewrite_shape_tokens("foo::other*", "foo", "bar", "app", "base"),
            None
        );
        assert_eq!(
            rewrite_shape_tokens("foo*", "foo", "bar", "app", "base"),
            None
        );
        // But the consumer's own bare `foo` IS the target when renaming in its own repo.
        assert_eq!(
            rewrite_shape_tokens("foo*", "foo", "bar", "app", "app"),
            Some("bar*".to_string())
        );
    }
}
