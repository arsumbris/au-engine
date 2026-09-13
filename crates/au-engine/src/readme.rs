//! Repo README obligations: the engine-owned `au.engine.readme` node at a repo's
//! root, and the diagnostics that keep it present, self-declared, and in place.
//!
//! A repo self-describes through a `README.md` at its root, a typed instance of
//! the hardwired `au.engine.readme` def (a body-only section template, see
//! [`crate::engine_schema`]). Three advisory `warning` checks:
//! - `repo-missing-readme`: an editable repo (the entry or an `edit` member) has
//!   no root `README.md`.
//! - `readme-type-undeclared`: a root `README.md` that does not self-declare
//!   `type: au.engine.readme::au-engine`.
//! - `readme-misplaced`: a file claiming `au.engine.readme` that is not a repo's
//!   root `README.md`.
//!
//! The obligation is scoped to AUTHORING surfaces (the entry and `edit` members);
//! a consumed `dep` / `discover` / cache snapshot is exempt, its README is its own
//! repo's concern.
//!
//! All README logic is localized here so a future declarable located-singleton
//! mechanism can fold `au.engine.readme` in as its first user, see
//! [[spec - repo readme - an engine-owned self-declaring node at the repo root with a required body template]].

use std::path::{Path, PathBuf};

use au_core::Instance;
use au_diagnostics::{ByteRange, Diagnostic, DiagnosticCode, Severity, Span, SuggestedFix};

use crate::ir::{FileEntry, OrdMap};
use crate::parse::FileParse;
use crate::repo::{RepoMap, RepoName, REGISTRY_REL};

/// The hardwired type a repo README self-declares.
pub(crate) const README_TYPE_NAME: &str = "au.engine.readme";

/// The canonical README basename, at a repo root.
const README_BASENAME: &str = "README.md";

/// An editable repo (the entry or an `edit` member) has no root `README.md`. A
/// repo self-describes through one, like it declares its identity through
/// `.arsumbris/repo.yaml`. Anchored at the repo's `repo.yaml`, since the missing
/// file has no span. `warning`, advisory, never blocks.
pub const REPO_MISSING_README: DiagnosticCode = DiagnosticCode::from_static("repo-missing-readme");

/// A root `README.md` that does not self-declare `type: au.engine.readme::au-engine`.
/// The canonical README self-declares its type; an undeclared one reads as a plain
/// note. `warning`, advisory.
pub const README_TYPE_UNDECLARED: DiagnosticCode =
    DiagnosticCode::from_static("readme-type-undeclared");

/// A file claiming `au.engine.readme` that is not a repo's root `README.md`. The
/// readme type is a singleton at `<root>/README.md`, so a claim elsewhere is a
/// misplacement. `warning`, advisory.
pub const README_MISPLACED: DiagnosticCode = DiagnosticCode::from_static("readme-misplaced");

/// The instance a catalog entry holds when it self-declares the readme type, else
/// `None` (a plain note, an asset, or an instance of some other type).
fn readme_claim(entry: &FileEntry) -> Option<&Instance> {
    match entry.parse.as_ref() {
        FileParse::Instance {
            instance: Some(inst),
            ..
        } if inst
            .type_claim
            .iter()
            .any(|c| c.name.as_str() == README_TYPE_NAME) =>
        {
            Some(inst)
        }
        _ => None,
    }
}

/// The README diagnostics for a build: the obligation and self-declaration over
/// each editable member's canonical README, plus the placement check over every
/// file that claims the readme type.
///
/// `editable_roots` is the entry plus every `edit` member, each paired with its
/// mounted root; a consumed or unmounted member is absent by construction, so it
/// is never on the hook.
pub(crate) fn readme_diagnostics(
    editable_roots: &[(RepoName, PathBuf)],
    repos: &RepoMap,
    catalog: &OrdMap<PathBuf, FileEntry>,
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    // Obligation + self-declaration, per editable member's canonical README.
    for (name, root) in editable_roots {
        let readme = root.join(README_BASENAME);
        match catalog.get(&readme) {
            None => diags.push(missing_readme_diag(name, root)),
            Some(entry) => {
                if readme_claim(entry).is_none() {
                    diags.push(type_undeclared_diag(&readme));
                }
            }
        }
    }

    // Placement, per file claiming the readme type: it must be its repo's root
    // README. The owning repo's root decides the canonical location.
    for (path, entry) in catalog.iter() {
        let Some(inst) = readme_claim(entry) else {
            continue;
        };
        let canonical = repos
            .repo_of(path)
            .map(|r| r.root.join(README_BASENAME))
            .is_some_and(|c| c == *path);
        if !canonical {
            let span = inst
                .type_claim
                .iter()
                .find(|c| c.name.as_str() == README_TYPE_NAME)
                .map(|c| c.span)
                .unwrap_or_else(|| ByteRange::new(0, 0));
            diags.push(misplaced_diag(path, span));
        }
    }

    diags
}

fn missing_readme_diag(name: &RepoName, root: &Path) -> Diagnostic {
    Diagnostic {
        code: REPO_MISSING_README,
        severity: Severity::Warning,
        span: Span::new(root.join(REGISTRY_REL), ByteRange::new(0, 0)),
        message: format!(
            "repo '{}' has no README.md at its root; a repo self-describes through a root README.md",
            name.as_str()
        ),
        related: vec![],
        fix: Some(SuggestedFix {
            description: "add a README.md at the repo root declaring `type: au.engine.readme::au-engine`, a `tldr:` field, and a `# Repo Overview` section holding `## What this is`, `## How to use this`, `## How to extend this`"
                .to_string(),
        }),
    }
}

fn type_undeclared_diag(readme: &Path) -> Diagnostic {
    Diagnostic {
        code: README_TYPE_UNDECLARED,
        severity: Severity::Warning,
        span: Span::new(readme.to_path_buf(), ByteRange::new(0, 0)),
        message:
            "README.md does not declare `type: au.engine.readme::au-engine`; the canonical README self-declares its type"
                .to_string(),
        related: vec![],
        fix: Some(SuggestedFix {
            description: "add `type: au.engine.readme::au-engine` to the README frontmatter"
                .to_string(),
        }),
    }
}

fn misplaced_diag(path: &Path, span: ByteRange) -> Diagnostic {
    Diagnostic {
        code: README_MISPLACED,
        severity: Severity::Warning,
        span: Span::new(path.to_path_buf(), span),
        message:
            "file claims `au.engine.readme` but is not the repo's root README.md; the readme type is a singleton at `<root>/README.md`"
                .to_string(),
        related: vec![],
        fix: Some(SuggestedFix {
            description: "move this to the repo root as README.md, or drop the `au.engine.readme` claim"
                .to_string(),
        }),
    }
}
