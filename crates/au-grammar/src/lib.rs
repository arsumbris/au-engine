//! Slot-expression parser.
//!
//! `parse_shape(&str) -> Result<Shape, ShapeParseError>` is the entry point.
//! The grammar is the slot-expression mini-language from [[type-def field shape::au-type-system]].
//! Recognizes [[type-def shape primitive::au-type-system]] primitives, [[type-def shape enum::au-type-system]] inline closed enums, [[type-def shape record::au-type-system]]
//! bare-name records, [[type-def shape suffixes::au-type-system]] typed references (`name*`, incl. `file*`),
//! [[type-def shape suffixes::au-type-system]] inline-or-reference (`name&`), the [[type-def shape suffixes::au-type-system]] list suffix `[]`,
//! [[type-def shape compound::au-type-system]] compound expressions (`<X | Y>`, `<X & Y>`), and the [[type-def shape suffixes::au-type-system]]
//! suffix matrix on compounds (`<...>*`, `<...>&`, `<...>[]`).
//!
//! `ShapeParseError` carries `code`, `severity`, and `message`; callers attach
//! the source `Span` to produce a full `Diagnostic`. au-grammar has no notion
//! of source files — it parses strings.

use std::fmt;

use au_diagnostics::{ByteRange, DiagnosticCode, Severity};
use serde::{Deserialize, Serialize};

/// Forward-compat hook for future deferred shape features. No
/// `parse_shape` path produces this code today, and no validate-time
/// path emits it for currently-supported shapes. au-core test helpers
/// still synthesize this code into `parsed_shape: Err(...)` to exercise
/// the lazy-surfacing-on-use mechanism (see `validate.rs::td()` and
/// `load_checks.rs::td_with_unparsed_field`); this keeps the lazy
/// dispatch wired and ready for any future shape feature that needs to
/// defer validate-time semantics. Removable in a future cleanup if
/// those test helpers migrate to `SHAPE_SYNTAX_ERROR` synthesis or are
/// retired with the introspection improvements they predate.
pub const NOT_YET_IMPLEMENTED_SHAPE_FEATURE: DiagnosticCode =
    DiagnosticCode::from_static("not-yet-implemented-shape-feature");
pub const SHAPE_SYNTAX_ERROR: DiagnosticCode = DiagnosticCode::from_static("shape-syntax-error");
/// A malformed value refinement ([[type-def field shape::au-type-system]], `Base{predicate}`): a
/// predicate invalid for its base, a duplicate-kind predicate, a bad number
/// literal, an unterminated regex, or an empty / non-primitive refinement. Also
/// emitted at load for a regex that does not compile or a bad temporal literal.
pub const REFINEMENT_BAD_SHAPE: DiagnosticCode =
    DiagnosticCode::from_static("refinement-bad-shape");
/// A malformed list cardinality suffix ([[type-def shape suffixes::au-type-system]], `T[x..y]`): an
/// inverted range (`[5..1]`), a non-integer or negative bound, or the redundant
/// `[..]`.
pub const CARDINALITY_BAD_SHAPE: DiagnosticCode =
    DiagnosticCode::from_static("cardinality-bad-shape");

/// Cap on shape-expression nesting depth. The recursive descent strips one
/// list suffix (`[]` / `[+]`) or one compound level (`<...>`) per recursion
/// step, every edge routing back through `parse_with_list_suffix`. The cap
/// bounds that recursion so an adversarial `.type.yaml` — `String` + a long
/// run of `[]`, or deeply nested `<...>` — produces a `shape-syntax-error`
/// instead of overflowing the stack and aborting the process. Set far above
/// any legitimate shape (real shapes nest a handful of levels at most),
/// mirroring au-core's `MAX_TYPE_CHAIN_DEPTH` and au-parser's `MAX_WALK_DEPTH`.
const MAX_SHAPE_DEPTH: usize = 64;

/// [[type-def shape primitive::au-type-system]].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Primitive {
    String,
    Number,
    Boolean,
    Date,
    DateTime,
    Url,
}

impl Primitive {
    pub fn as_str(self) -> &'static str {
        match self {
            Primitive::String => "String",
            Primitive::Number => "Number",
            Primitive::Boolean => "Boolean",
            Primitive::Date => "Date",
            Primitive::DateTime => "DateTime",
            Primitive::Url => "Url",
        }
    }
}

/// A value refinement on a scalar primitive base ([[type-def field shape::au-type-system]]),
/// the `{predicate}` in `Number{>=0 & integer}` / `String{/^[a-z]+$/}`. A MEET
/// on the base's value lattice: comparison bounds, an `integer` flag, and a
/// regex pattern, at most one of each. A numeric bound's literal is stored
/// canonicalized (shortest exact decimal, so `>=0` and `>=0.0` are one identity);
/// a temporal bound stores the source literal, whose calendar validity au-core
/// checks. `pattern` is the text between the `/.../` delimiters verbatim (a
/// literal slash stays `\/`); au-core compiles it and enforces the regular subset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Refinement {
    pub lower: Option<Bound>,
    pub upper: Option<Bound>,
    pub integer: bool,
    pub pattern: Option<String>,
}

/// One comparison bound in a [`Refinement`]. `inclusive` is `>=` / `<=` (true)
/// versus the strict `>` / `<` (false). `value` is the canonical literal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Bound {
    pub value: String,
    pub inclusive: bool,
}

/// [[type-def field shape::au-type-system]]. `Enum` literals are stored in declaration order — token
/// equality ([[type-def fields collision - auto-unify and qualified field::au-type-system]]) is structural over this `Vec`, so `[a, b] != [b, a]`.
/// Same ordering rule applies to `Union`/`Intersection` branches and
/// `CompoundReference.branches`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Shape {
    Primitive(Primitive),
    /// Inline closed enum ([[type-def shape enum::au-type-system]]). Each entry is a literal value as it
    /// appeared on the source side, after whitespace trimming around commas.
    /// Whitespace inside a literal is rejected; quoted literals are not yet
    /// supported.
    Enum(Vec<String>),
    /// The no-type slot ([[type-def shape any::au-type-system]], bare `any`). Holds an inline value of any
    /// shape, stored verbatim and uninterpreted: no shape check, no
    /// nested-`type:` claim, no wikilink scan, no candidate scan. The reference
    /// forms `any*` / `any&` are not this variant — they parse as
    /// `Shape::Reference("any")` / `Shape::InlineOrReference("any")`, the `"any"`
    /// sentinel resolves engine-side like `"file"`, any node by existence with
    /// no closure check ([[type-def shape any::au-type-system]]).
    Any,
    /// The uninterpreted inline slot ([[type-def shape opaque::au-type-system]], bare `opaque`).
    /// Holds an inline value of any shape, stored VERBATIM and never read: no
    /// shape check, no nested-`type:` claim, no candidate scan. The interpreted
    /// top type is `Shape::Any`; `opaque` is its uninterpreted sibling. Inline
    /// only — `opaque*` / `opaque&` are rejected, a reference is interpreted by
    /// definition, so the top-type reference is `any*` / `any&`. The list form
    /// `opaque[]` / `opaque[+]` is a `Shape::List` wrapping this variant. The
    /// wikilink/edge suppression is the intent and not yet wired, see
    /// [[type-def shape opaque::au-type-system]].
    Opaque,
    /// Typed reference ([[type-def shape suffixes::au-type-system]], `name*`). Target file's `type:` closure
    /// must include `name`. Built-in `file` is an in-grammar literal but
    /// resolves engine-side without a closure check ([[type-def shape file::au-type-system]]). The string
    /// is the type-def name as written, optionally `::repo`-qualified; au-core
    /// enforces the [[type-def legal names::au-type-system]] regex and existence in the type graph.
    Reference(QualifiedName),
    /// Bare-name record slot ([[type-def shape record::au-type-system]], e.g. `rationale`). Value must be
    /// an inline YAML map ([[type-def shape record::au-type-system]]); the slot's demanded type-def supplies the
    /// effective shape. Distinct from `Shape::Reference` (which is `name*`
    /// and wants a wikilink) and `Shape::InlineOrReference` (which is
    /// `name&` and accepts both).
    Record(QualifiedName),
    /// Bare-name inline-or-reference slot ([[type-def shape suffixes::au-type-system]], `name&`). Value may
    /// be an inline map ([[type-def shape record::au-type-system]]) or a `[[wikilink]]` string whose target's
    /// closure includes `name`. The single-name parallel to
    /// `Shape::CompoundReference { mode: Inline, .. }`.
    InlineOrReference(QualifiedName),
    /// List of values matching the wrapped shape. [[type-def shape suffixes::au-type-system]].
    /// Range cardinality: `min` is the inclusive lower bound on the element
    /// count, `max` the inclusive upper (`None` is unbounded above). The source
    /// suffixes desugar here: `[]` is `{min:0, max:None}`, `[+]` is
    /// `{min:1, max:None}`, `[n]` is `{min:n, max:Some(n)}`, `[..m]` is
    /// `{min:0, max:Some(m)}`, `[x..]` is `{min:x, max:None}`, `[x..y]` is
    /// `{min:x, max:Some(y)}`. The list suffix attaches outermost; nested lists
    /// (`String[][]`) fall out of the recursive grammar without special handling.
    List {
        inner: Box<Shape>,
        min: u32,
        max: Option<u32>,
    },
    /// Slot-level union ([[type-def shape compound::au-type-system]], `<X | Y>`). Branch order is preserved
    /// — `<A | B>` ≠ `<B | A>` per the [[type-def fields collision - auto-unify and qualified field::au-type-system]] structural-equality rule. The
    /// AST allows N branches: `<A | B | C>` parses as one `Union` with
    /// three entries (chained, not nested).
    Union(Vec<Shape>),
    /// Slot-level intersection ([[type-def shape compound::au-type-system]], `<X & Y>`). Same N-ary,
    /// order-significant shape as `Union`.
    Intersection(Vec<Shape>),
    /// Reference suffix attached to a compound expression ([[type-def shape suffixes::au-type-system]]):
    /// `<X | Y>*` / `<X | Y>&` (Union of branches) or `<X & Y>*` /
    /// `<X & Y>&` (Intersection). `branches` are bare type-def names —
    /// primitives and inline enums in the compound are rejected at parse
    /// time per [[type-def shape suffixes::au-type-system]] ("operands are all type-def names"). Validation
    /// semantics: the value's reference target's `type:` closure must
    /// satisfy the compound (any-of for Union, all-of for Intersection).
    /// `Inline` mode (`&`) defers validation to the inline-value phase.
    CompoundReference {
        mode: RefMode,
        op: CompoundRefOp,
        branches: Vec<QualifiedName>,
    },
    /// Typed reference to a type-def ([[type-def shape def-ref::au-type-system]], `type<T>*` /
    /// `type*`). The def-axis sibling of `Shape::Reference` (`T*`): the value is
    /// a wikilink to a `.type.yaml` file, and the constraint is checked against
    /// that target def's [[type-def type]] parent closure, not an instance's
    /// identity closure. `None` is the unconstrained `type*` (any type-def by
    /// existence, the def-axis `any*`). Reference-only — the `*` is part of the
    /// keyword form, there is no `type<T>` or `type<T>&`. The `type` keyword is
    /// reserved in reference position; `type*` here is the def-ref, never a
    /// reference to a type-def literally named `type`.
    DefReference(Option<DefBound>),
    /// Commit-pinned reference ([[type-def shape suffixes::au-type-system]], the `@` postfix). Wraps a
    /// `*` reference-bearing inner shape and demands every value carry a `@commit`
    /// pin: `file*@`, `any*@`, `T*@`, `<A | B>*@`, `type<T>*@`. `@` attaches only
    /// to `*`, so `T&@` is a `shape-syntax-error` (a `&` inline branch cannot pin,
    /// write the union `< T& | T*@ >`), as is `@` on a primitive, enum, or bare
    /// record. The `@` attaches after the reference suffix and before the list
    /// suffix, so `T*@[]` is `List(Pinned(Reference))` and the inner is never a
    /// list. A pin is an INERT snapshot, validation enforces the pin is present
    /// but does not re-resolve or drift-check it, see
    /// [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
    Pinned(Box<Shape>),
    /// Value refinement on a scalar primitive ([[type-def field shape::au-type-system]], the
    /// `Base{predicate}` form, e.g. `Number{>=0 & integer}`). `base` is the
    /// refinable primitive, `refinement` the meet narrowing it. Inline only: a
    /// refined primitive is not reference-bearing and takes no `*` / `&` / `@`.
    Refined {
        base: Primitive,
        refinement: Refinement,
    },
    /// Tuple, a fixed-arity positional product ([[type-def shape refinement::au-type-system]] sibling,
    /// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]):
    /// `(A, B, ...)`, every element required, each a full shape. Element order is
    /// significant — `(String, Number)` ≠ `(Number, String)` — like `Enum` /
    /// `Union` branches. `(...)` is always a tuple, `[...]` always a list, so
    /// there is no encoding collision. Inline only (nominal), so it takes no
    /// `*` / `&`.
    Tuple(Vec<Shape>),
}

/// The constraint inside a `type<...>*` def-reference ([[type-def shape def-ref::au-type-system]]).
/// Mirrors the compound-of-names that `*` / `&` accept, on the def axis: the
/// named ceilings are matched against the target type-def's parent closure.
/// Branch order is preserved, like `Union` / `CompoundReference.branches`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DefBound {
    /// `type<T>*` — one ceiling. The target def's parent closure must include `T`.
    Single(QualifiedName),
    /// `type<a | b>*` / `type<a & b>*` — a compound of ceilings. `Union` is
    /// any-of, `Intersection` is all-of, over the target def's parent closure.
    Compound {
        op: CompoundRefOp,
        branches: Vec<QualifiedName>,
    },
}

/// Suffix kind on a compound reference (`*` or `&`). [[type-def shape suffixes::au-type-system]].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RefMode {
    /// `<...>*` — typed reference. Value must be a wikilink; target's
    /// `type:` closure is checked against the compound.
    Star,
    /// `<...>&` — inline-or-reference. Value may be a wikilink (resolved
    /// as `Star`) or an inline record. Inline-record validation is
    /// deferred to a later phase.
    Inline,
}

impl RefMode {
    /// The character that produces this mode in the source form.
    pub fn suffix_char(self) -> char {
        match self {
            RefMode::Star => '*',
            RefMode::Inline => '&',
        }
    }
}

/// Compound operator on a compound reference. Mirrors `Shape::Union` /
/// `Shape::Intersection` for the suffix-bearing case but flattens the
/// branches to bare names per [[type-def shape suffixes::au-type-system]].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CompoundRefOp {
    /// `<X | Y>` — target's closure satisfies any of `branches`.
    Union,
    /// `<X & Y>` — target's closure satisfies all of `branches`.
    Intersection,
}

impl CompoundRefOp {
    pub fn separator_char(self) -> char {
        match self {
            CompoundRefOp::Union => '|',
            CompoundRefOp::Intersection => '&',
        }
    }
}

impl fmt::Display for Primitive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A type-def name in a shape, optionally carrying a `::repo` peer qualifier.
///
/// `repo: None` is a name in the authoring repo's own graph (bare `foo`).
/// `repo: Some(r)` is a peer's type (`foo::r`), resolved against repo `r`'s
/// graph at fold time. See [[design - cross-repo type vocabulary - reference import and vendor as one spectrum over the repo qualifier]].
///
/// The `::` delimiter is unambiguous in shape position: `:` is not a shape
/// operator and is illegal inside a [[type-def legal names::au-type-system]] name, so a `::`
/// can only be the repo qualifier. The base is the [[type-def legal names::au-type-system]]
/// type-name; the repo is a single-segment identifier.
///
/// Serializes transparently as its source string (`base` or `base::repo`), so
/// the [[spec - shape ast on the wire - the parsed slot shape as a tagged union beside the source string]]
/// contract stays a string — an unqualified name is byte-identical to before,
/// a qualified one is just a longer name string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QualifiedName {
    pub base: String,
    pub repo: Option<String>,
}

impl QualifiedName {
    /// A name in the authoring repo's own graph, no `::repo` qualifier.
    pub fn own(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            repo: None,
        }
    }

    /// The base type-def name, ignoring any `::repo` qualifier. Consumers that
    /// resolve against a single graph read this; repo-aware resolution reads
    /// `repo` explicitly. Keeps name-keyed lookups drop-in across the switch
    /// from a bare `String`.
    pub fn as_str(&self) -> &str {
        &self.base
    }

    /// Re-qualify a BARE user-type name to `repo`, returning a copy. A built-in
    /// (`file` / `any` / `type` / a primitive) or an already-`::`-qualified name
    /// is returned unchanged.
    ///
    /// The transform is EXACT, not heuristic: a cross-repo reference is always
    /// authored with a `::repo` qualifier, so a bare user-type name in a def's
    /// shape is definitionally that def's OWN repo's type. Rewriting it to `repo`
    /// reinterprets a peer def's field from the peer's perspective into a
    /// consumer's, which is what a folded peer field needs. See
    /// [[design - cross-repo type vocabulary - reference import and vendor as one spectrum over the repo qualifier]].
    pub fn qualified_to(&self, repo: &str) -> QualifiedName {
        if self.repo.is_none() && !is_reserved_builtin(&self.base) {
            QualifiedName {
                base: self.base.clone(),
                repo: Some(repo.to_string()),
            }
        } else {
            self.clone()
        }
    }
}

/// Parses the source form, splitting on the first `::`. No validation — the
/// `parse_shape` path validates both halves; this is the ergonomic
/// construction path for callers (tests, fixtures, deserialization) holding a
/// trusted string.
impl From<&str> for QualifiedName {
    fn from(s: &str) -> Self {
        match s.split_once("::") {
            Some((base, repo)) => Self {
                base: base.to_string(),
                repo: Some(repo.to_string()),
            },
            None => Self::own(s),
        }
    }
}

impl From<String> for QualifiedName {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}

impl fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.repo {
            Some(repo) => write!(f, "{}::{}", self.base, repo),
            None => f.write_str(&self.base),
        }
    }
}

impl Serialize for QualifiedName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for QualifiedName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from(s))
    }
}

impl Shape {
    /// Re-qualify every BARE user-type name in this shape to `repo`, recursively.
    /// Built-ins (`file` / `any` / `type` / primitives), enums, and already-
    /// `::`-qualified names are left unchanged.
    ///
    /// Used to reinterpret a FOLDED peer type's field shapes, whose bare names are
    /// the peer's OWN types, from a consumer repo's perspective: a `base` type's
    /// `child: baz*` field reads as `child: baz::base*` once folded into a source
    /// repo, so the reference resolves against `base`, not the source. See
    /// [[design - cross-repo type vocabulary - reference import and vendor as one spectrum over the repo qualifier]].
    ///
    /// A `DefReference` bound (`type<T>*`) is re-qualified too, its ceiling is a
    /// type-name position like any other; its consumer resolves a `::repo` ceiling
    /// on the def axis.
    pub fn qualify_bare(&self, repo: &str) -> Shape {
        match self {
            Shape::Reference(n) => Shape::Reference(n.qualified_to(repo)),
            Shape::Record(n) => Shape::Record(n.qualified_to(repo)),
            Shape::InlineOrReference(n) => Shape::InlineOrReference(n.qualified_to(repo)),
            Shape::List { inner, min, max } => Shape::List {
                inner: Box::new(inner.qualify_bare(repo)),
                min: *min,
                max: *max,
            },
            Shape::Union(branches) => {
                Shape::Union(branches.iter().map(|b| b.qualify_bare(repo)).collect())
            }
            Shape::Intersection(branches) => {
                Shape::Intersection(branches.iter().map(|b| b.qualify_bare(repo)).collect())
            }
            Shape::CompoundReference { mode, op, branches } => Shape::CompoundReference {
                mode: *mode,
                op: *op,
                branches: branches.iter().map(|b| b.qualified_to(repo)).collect(),
            },
            Shape::DefReference(bound) => {
                Shape::DefReference(bound.as_ref().map(|b| b.qualify_bare(repo)))
            }
            Shape::Pinned(inner) => Shape::Pinned(Box::new(inner.qualify_bare(repo))),
            // A tuple's elements may name user types (records), re-qualified like
            // any nested shape.
            Shape::Tuple(elements) => {
                Shape::Tuple(elements.iter().map(|e| e.qualify_bare(repo)).collect())
            }
            // No user-type name to re-qualify.
            Shape::Primitive(_)
            | Shape::Enum(_)
            | Shape::Any
            | Shape::Opaque
            | Shape::Refined { .. } => self.clone(),
        }
    }
}

impl DefBound {
    /// Re-qualify a def-reference bound's BARE ceiling names to `repo` (see
    /// [`QualifiedName::qualified_to`]), the def-axis parallel of
    /// [`Shape::qualify_bare`].
    pub fn qualify_bare(&self, repo: &str) -> DefBound {
        match self {
            DefBound::Single(n) => DefBound::Single(n.qualified_to(repo)),
            DefBound::Compound { op, branches } => DefBound::Compound {
                op: *op,
                branches: branches.iter().map(|b| b.qualified_to(repo)).collect(),
            },
        }
    }
}

/// Renders the Shape as its source-form slot expression — the same string
/// `parse_shape` would produce this AST from. Used by diagnostic messages
/// and JSON introspection output.
impl fmt::Display for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Shape::Primitive(p) => write!(f, "{}", p),
            Shape::Any => f.write_str("any"),
            Shape::Opaque => f.write_str("opaque"),
            Shape::Enum(literals) => write!(f, "[{}]", literals.join(", ")),
            Shape::Reference(name) => write!(f, "{}*", name),
            Shape::Record(name) => write!(f, "{}", name),
            Shape::InlineOrReference(name) => write!(f, "{}&", name),
            Shape::List { inner, min, max } => {
                write!(f, "{}", inner)?;
                write_list_suffix(f, *min, *max)
            }
            Shape::Union(branches) => write_compound(f, branches, '|'),
            Shape::Intersection(branches) => write_compound(f, branches, '&'),
            Shape::CompoundReference { mode, op, branches } => {
                // Render each branch with an explicit `*` suffix. The
                // spec-preferred bare-name form (`<a | b>*`) also
                // roundtrips through `parse_shape` since bare records
                // landed; `parse_compound_with_suffix` collapses both
                // forms to the same `branches: Vec<String>` AST. We
                // emit the `*`-per-branch form here for stable test
                // assertions and snapshot stability — switching to
                // bare-name output would invalidate existing snapshots
                // without changing AST equivalence.
                f.write_str("<")?;
                for (i, name) in branches.iter().enumerate() {
                    if i > 0 {
                        write!(f, " {} ", op.separator_char())?;
                    }
                    write!(f, "{}*", name)?;
                }
                write!(f, ">{}", mode.suffix_char())
            }
            Shape::DefReference(None) => f.write_str("type*"),
            Shape::DefReference(Some(DefBound::Single(name))) => write!(f, "type<{}>*", name),
            Shape::DefReference(Some(DefBound::Compound { op, branches })) => {
                f.write_str("type<")?;
                for (i, name) in branches.iter().enumerate() {
                    if i > 0 {
                        write!(f, " {} ", op.separator_char())?;
                    }
                    write!(f, "{}", name)?;
                }
                f.write_str(">*")
            }
            Shape::Pinned(inner) => write!(f, "{}@", inner),
            Shape::Refined { base, refinement } => {
                write!(f, "{}", base)?;
                write_refinement(f, refinement)
            }
            Shape::Tuple(elements) => {
                f.write_str("(")?;
                for (i, el) in elements.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", el)?;
                }
                f.write_str(")")
            }
        }
    }
}

/// Render a value refinement `{predicate}` in canonical order: lower comparison,
/// upper comparison, `integer`, regex, joined by ` & `. A literal slash in a
/// regex renders as its stored `\/`, so the form round-trips through `parse_shape`.
fn write_refinement(f: &mut fmt::Formatter<'_>, r: &Refinement) -> fmt::Result {
    let mut parts: Vec<String> = Vec::new();
    if let Some(b) = &r.lower {
        parts.push(format!(
            "{}{}",
            if b.inclusive { ">=" } else { ">" },
            b.value
        ));
    }
    if let Some(b) = &r.upper {
        parts.push(format!(
            "{}{}",
            if b.inclusive { "<=" } else { "<" },
            b.value
        ));
    }
    if r.integer {
        parts.push("integer".to_string());
    }
    if let Some(p) = &r.pattern {
        parts.push(format!("/{}/", p));
    }
    write!(f, "{{{}}}", parts.join(" & "))
}

/// Render a list-cardinality suffix in its canonical source form. `[]` / `[+]`
/// stay the sugar for `{0,None}` / `{1,None}`, an exact `{n,Some(n)}` is `[n]`,
/// and the open / bounded forms render `[x..]` / `[..m]` / `[x..y]`. The choice
/// is total over `(min, max)`, so identity is deterministic.
fn write_list_suffix(f: &mut fmt::Formatter<'_>, min: u32, max: Option<u32>) -> fmt::Result {
    match (min, max) {
        (n, Some(m)) if m == n => write!(f, "[{n}]"),
        (0, None) => f.write_str("[]"),
        (1, None) => f.write_str("[+]"),
        (x, None) => write!(f, "[{x}..]"),
        (0, Some(m)) => write!(f, "[..{m}]"),
        (x, Some(m)) => write!(f, "[{x}..{m}]"),
    }
}

/// The canonical source-form list-cardinality suffix for `(min, max)` as an
/// owned string, e.g. `[]`, `[+]`, `[3]`, `[3..]`, `[..5]`, `[2..5]`. Delegates
/// to [`write_list_suffix`], so consumer-crate diagnostics render the same form
/// as [`Shape`]'s `Display` with no drift.
pub fn list_suffix_string(min: u32, max: Option<u32>) -> String {
    struct Suffix(u32, Option<u32>);
    impl fmt::Display for Suffix {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write_list_suffix(f, self.0, self.1)
        }
    }
    Suffix(min, max).to_string()
}

fn write_compound(f: &mut fmt::Formatter<'_>, branches: &[Shape], op: char) -> fmt::Result {
    f.write_str("<")?;
    for (i, branch) in branches.iter().enumerate() {
        if i > 0 {
            write!(f, " {} ", op)?;
        }
        write!(f, "{}", branch)?;
    }
    f.write_str(">")
}

/// Failure from `parse_shape`. Spanless — the caller (au-core graph build)
/// pairs this with the field's `shape_span` to make a full `Diagnostic`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeParseError {
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub message: String,
}

/// The role of a span inside a parsed shape, for semantic-token classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeSpanRole {
    /// A navigable type-def name: a record / reference / inline-or-reference
    /// base, a compound operand, or a `type<T>*` def-ref bound.
    TypeName,
    /// A built-in shape keyword: a primitive, `file`, `any`, or `type`.
    Builtin,
    /// An inline enum literal.
    EnumMember,
}

/// A classified byte span inside a parsed shape.
///
/// Rides ALONGSIDE the structural [`Shape`], never inside its variants. `Shape`
/// derives structural equality and auto-unify compares whole shapes, so a span
/// inside a variant would make two identical shapes at different offsets compare
/// unequal (a false `mixin-collision`). The range is relative to the string
/// passed to [`parse_shape_spanned`]; the caller rebases it onto the field's
/// shape-span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeSpan {
    pub range: ByteRange,
    pub role: ShapeSpanRole,
}

/// Collects [`ShapeSpan`]s during a span-aware parse. `origin` is the full
/// string the spans are relative to. Every token the parser handles is a
/// subslice of `origin`, so a token's byte offset is `tok.as_ptr() -
/// origin.as_ptr()`, recovered without threading offsets through the recursion.
struct SpanCtx<'a> {
    origin: &'a str,
    spans: Vec<ShapeSpan>,
}

impl SpanCtx<'_> {
    /// Record `tok`'s span. `tok` MUST be a subslice of `self.origin`.
    ///
    /// The offset is recovered by pointer subtraction, so a non-subslice would
    /// underflow `usize` and wrap to a garbage span in release. Every current
    /// caller passes a subslice of the trimmed input; the bounds guard makes a
    /// future non-subslice a dropped span rather than a silent corruption.
    fn record(&mut self, tok: &str, role: ShapeSpanRole) {
        let origin_start = self.origin.as_ptr() as usize;
        let tok_start = tok.as_ptr() as usize;
        if tok_start < origin_start || tok_start + tok.len() > origin_start + self.origin.len() {
            debug_assert!(false, "tok not a subslice of origin");
            return;
        }
        let start = tok_start - origin_start;
        self.spans.push(ShapeSpan {
            range: ByteRange::new(start, start + tok.len()),
            role,
        });
    }
}

/// The role a bare reference / record name plays: `file` / `any` / `type` and
/// the primitives are built-in keywords, everything else is a type-def name.
fn shape_name_role(name: &str) -> ShapeSpanRole {
    if is_primitive_name(name)
        || name == "file"
        || name == "any"
        || name == "opaque"
        || name == "type"
    {
        ShapeSpanRole::Builtin
    } else {
        ShapeSpanRole::TypeName
    }
}

/// Parse a slot-expression string into a `Shape`.
///
/// Recognized today:
/// - `String` / `Number` / `Boolean` / `Date` / `DateTime` → `Shape::Primitive`
/// - `[v1, v2, ...]` → `Shape::Enum`
/// - bare `any` → `Shape::Any` ([[type-def shape any::au-type-system]], the no-type inline slot)
/// - bare `name` → `Shape::Record` ([[type-def shape record::au-type-system]] record-slot, inline only)
/// - `name*` (incl. `file*`, `decision.decided*`) → `Shape::Reference`
/// - `name&` → `Shape::InlineOrReference` (inline or wikilink)
/// - `<inner>[]` → `Shape::List` (where `<inner>` is any recognized shape)
/// - `<X | Y>` → `Shape::Union`, `<X & Y>` → `Shape::Intersection` ([[type-def shape compound::au-type-system]])
/// - `<X | Y>*` / `<X & Y>*` / `<X | Y>&` / `<X & Y>&` → `Shape::CompoundReference`
/// - `type*` / `type<T>*` / `type<a | b>*` / `type<a & b>*` → `Shape::DefReference` ([[type-def shape def-ref::au-type-system]])
/// - `<ref>*@` (e.g. `file*@`, `T*@`, `<A | B>*@`, `type<T>*@`) → `Shape::Pinned` ([[type-def shape suffixes::au-type-system]], the commit-pin postfix; `@` requires `*`, so `T&@` is rejected)
///
/// Rejected (`shape-syntax-error`):
/// - `@` on a non-reference (`String@`, `[a, b]@`, bare `T@`), a double `@` (`T*@@`)
/// - `@` on a `&` inline-or-reference (`T&@`): `@` requires `*`, a `&` inline branch cannot pin (write `< T& | T*@ >`)
/// - Empty / whitespace-only / leading non-identifier
/// - `*` / `&` on primitives, inline enums, or compounds containing them
/// - Bare `file` and `file&` ([[type-def shape file::au-type-system]]: `file` is meaningful only with `*`)
/// - `type<T>` / `type<T>&` ([[type-def shape def-ref::au-type-system]]: def-refs are reference-only, `*` is part of the keyword)
/// - Empty / single-branch compounds, mixed `|`/`&` operators in one compound
/// - Names violating the [[type-def legal names::au-type-system]] type-name regex
///
/// `not-yet-implemented-shape-feature` is reserved for future deferred
/// features but no `parse_shape` path produces it today.
pub fn parse_shape(raw: &str) -> Result<Shape, ShapeParseError> {
    parse_shape_spanned(raw).0
}

/// Parse a slot-expression string into a `Shape`, AND collect the classified
/// byte spans of every type-def name, built-in keyword, and enum literal inside
/// it (see [`ShapeSpan`]). Spans are relative to `raw` and ride alongside the
/// `Shape`, never inside it. On a parse error the span list is empty.
///
/// Powers the `semantic_tokens` type-def shape layer. `parse_shape` delegates
/// here and drops the spans, so the grammar has one parser, no drift.
pub fn parse_shape_spanned(raw: &str) -> (Result<Shape, ShapeParseError>, Vec<ShapeSpan>) {
    let mut ctx = SpanCtx {
        origin: raw,
        spans: Vec::new(),
    };
    let result = parse_shape_core(raw, &mut ctx);
    if result.is_err() {
        // Partial spans from a path that then failed are meaningless; the
        // field-shape token serves a null `value_type` and no leaves.
        ctx.spans.clear();
    }
    (result, ctx.spans)
}

fn parse_shape_core(raw: &str, ctx: &mut SpanCtx) -> Result<Shape, ShapeParseError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(syntax_error("shape is empty"));
    }
    // Pre-check bracket balance so accidental typos like `<X>Y>`,
    // `<a* | b*` (no closing), or `[a, b` produce a focused diagnostic
    // instead of routing through deeper parsers that would surface a
    // misleading "single branch" or "shape uses a deferred feature"
    // message. Recursive callers (parse_compound, parse_compound_with_suffix)
    // re-check the post-strip inner so cases like `<>X<>` (globally
    // balanced but inner-imbalanced after stripping outer brackets) also
    // get a clear error.
    if let Err(reason) = bracket_balance(trimmed) {
        return Err(syntax_error(format!(
            "shape has unbalanced brackets: {reason}"
        )));
    }
    parse_with_list_suffix(trimmed, 0, ctx)
}

/// [[type-def shape suffixes::au-type-system]]: `[]` (and its atomic non-empty variant `[+]`) attach last
/// (outermost). Strip any number of trailing `[]` / `[+]` first and recurse,
/// then dispatch to `parse_unlisted` for the inner shape. Falls through
/// cleanly for non-list inputs.
///
/// `depth` is the current recursion depth. Both recursion sources — the
/// list-suffix self-call below and the per-branch descent in `parse_compound`
/// — re-enter here with `depth + 1`, so the `MAX_SHAPE_DEPTH` check guards
/// the whole grammar against stack overflow on adversarial input.
fn parse_with_list_suffix(
    s: &str,
    depth: usize,
    ctx: &mut SpanCtx,
) -> Result<Shape, ShapeParseError> {
    if depth > MAX_SHAPE_DEPTH {
        return Err(syntax_error("shape nesting is too deep"));
    }
    if let Some((prefix, content)) = split_trailing_list_suffix(s) {
        let prefix = prefix.trim_end();
        if prefix.is_empty() {
            return Err(syntax_error("list cardinality suffix has no inner shape"));
        }
        let (min, max) = parse_cardinality(content)?;
        let inner = parse_with_list_suffix(prefix, depth + 1, ctx)?;
        return Ok(Shape::List {
            inner: Box::new(inner),
            min,
            max,
        });
    }
    parse_unlisted(s, depth, ctx)
}

/// If `s` ends with a top-level `[...]` list-cardinality suffix, split it into
/// `(prefix, content)`, the shape before the bracket and the text between the
/// brackets. Returns `None` when `s` does not end in `]`, or the matching `[`
/// sits at index 0 — a bare `[...]` is a standalone enum or atom, handled by
/// `parse_atom`, never a suffix.
///
/// The backward scan counts only square brackets and stops at the `[` that
/// matches the final `]`, so a refinement `{...}` in the prefix (which ends in
/// `}`, and whose inner `[` / `]` never reach the scan) is never mistaken for
/// the suffix. `String{/[a-z]/}[3]` splits into `String{/[a-z]/}` and `3`.
fn split_trailing_list_suffix(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    if bytes.last() != Some(&b']') {
        return None;
    }
    let mut depth: i32 = 0;
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b']' => depth += 1,
            b'[' => {
                depth -= 1;
                if depth == 0 {
                    if i == 0 {
                        return None;
                    }
                    return Some((&s[..i], &s[i + 1..bytes.len() - 1]));
                }
            }
            _ => {}
        }
    }
    None
}

/// Parse the content of a `[...]` list-cardinality suffix into an inclusive
/// `(min, max)` count bound. [[type-def shape suffixes::au-type-system]]. The sugar and range
/// forms: `[]` is `(0, None)`, `[+]` is `(1, None)`, `[n]` is `(n, Some(n))`,
/// `[x..]` is `(x, None)`, `[..m]` is `(0, Some(m))`, `[x..y]` is
/// `(x, Some(y))`. `[..]` is rejected as redundant with `[]`, an inverted
/// `[x..y]` with `x > y` is rejected, and a non-integer or negative bound is
/// rejected.
fn parse_cardinality(content: &str) -> Result<(u32, Option<u32>), ShapeParseError> {
    let c = content.trim();
    if c.is_empty() {
        return Ok((0, None));
    }
    if c == "+" {
        return Ok((1, None));
    }
    if let Some((lo, hi)) = c.split_once("..") {
        let (lo, hi) = (lo.trim(), hi.trim());
        if lo.is_empty() && hi.is_empty() {
            return Err(cardinality_bad_shape(
                "'[..]' is redundant with '[]'; use '[]'",
            ));
        }
        let min = if lo.is_empty() { 0 } else { parse_count(lo)? };
        let max = if hi.is_empty() {
            None
        } else {
            Some(parse_count(hi)?)
        };
        if let Some(m) = max {
            if min > m {
                return Err(cardinality_bad_shape(format!(
                    "list cardinality '[{min}..{m}]' is inverted; the lower bound must not exceed the upper"
                )));
            }
        }
        return Ok((min, max));
    }
    let n = parse_count(c)?;
    Ok((n, Some(n)))
}

fn parse_count(s: &str) -> Result<u32, ShapeParseError> {
    s.parse::<u32>().map_err(|_| {
        cardinality_bad_shape(format!(
            "list cardinality bound '{s}' is not a non-negative integer"
        ))
    })
}

/// Parse a shape expression that has had any outer `[]` already stripped.
/// Handles the optional ref suffix (`*` / `&`) and dispatches to `parse_atom`
/// when no suffix is present. `depth` is forwarded unchanged — this is a
/// dispatch step, not a nesting level.
fn parse_unlisted(s: &str, depth: usize, ctx: &mut SpanCtx) -> Result<Shape, ShapeParseError> {
    // [[type-def shape suffixes::au-type-system]]: the `@` pin-enforcement attaches after the
    // reference suffix and before the list suffix, so it strips here first,
    // outside the `*` / `&`. `T*@[]` already had its `[]` stripped upstream, so
    // the inner under `@` is never a list.
    if let Some(prefix) = s.strip_suffix('@') {
        return parse_pinned(prefix.trim_end(), depth, ctx);
    }
    if let Some(prefix) = s.strip_suffix('*') {
        return parse_reference(prefix.trim_end(), depth, ctx);
    }
    if let Some(prefix) = s.strip_suffix('&') {
        return parse_inline_or_ref(prefix.trim_end(), depth, ctx);
    }
    parse_atom(s, depth, ctx)
}

/// Inner side of the `@` pin-enforcement postfix ([[type-def shape suffixes::au-type-system]]). The
/// `@` attaches only to a `*` reference — never to a primitive, enum, bare
/// record, or a `&` inline-or-reference. `T&@` is a contradiction, the `@`
/// demands a pin while `&`'s inline branch cannot carry one, so it is rejected;
/// the intent is the union `< T& | T*@ >`. A second `@` (`T*@@`) and a `@` with
/// no inner are both rejected. The inner parses through `parse_unlisted`, which
/// strips its `*` / `&`, then the result is checked for reference-shape-ness.
fn parse_pinned(prefix: &str, depth: usize, ctx: &mut SpanCtx) -> Result<Shape, ShapeParseError> {
    if prefix.is_empty() {
        return Err(syntax_error("'@' pin-enforcement has no inner shape"));
    }
    if prefix.ends_with('@') {
        return Err(syntax_error(
            "'@' pin-enforcement is mutually exclusive and may appear once",
        ));
    }
    let inner = parse_unlisted(prefix, depth, ctx)?;
    if let Shape::InlineOrReference(name) = &inner {
        return Err(syntax_error(format!(
            "'@' pin-enforcement requires a '*' reference, not a '&' inline-or-reference ('{prefix}'); a '&' inline branch cannot pin — write the union '< {name}& | {name}*@ >'"
        )));
    }
    if !is_reference_bearing(&inner) {
        return Err(syntax_error(format!(
            "'@' pin-enforcement requires a reference shape; '{}' is not a reference",
            prefix
        )));
    }
    Ok(Shape::Pinned(Box::new(inner)))
}

/// Whether a shape is reference-bearing, the only kind the `@` pin and the
/// reference suffixes attach to. A list, primitive, enum, bare record, inline
/// `any`, or anonymous compound is not.
/// The element shape a slot ultimately demands, unwrapping the `[]` / `[+]`
/// list and `*@` pin wrappers. `T*`, `T*[]`, `T*@`, and `T*@[]` all yield the
/// same inner `T*`.
///
/// Wrapper order is fixed by [[type-def shape suffixes::au-type-system]] (reference suffix,
/// then `@`, then the list suffix), but this loops rather than assuming a
/// depth, so a future wrapper composes without changing callers.
pub fn slot_element_shape(shape: &Shape) -> &Shape {
    let mut cur = shape;
    loop {
        match cur {
            Shape::List { inner, .. } | Shape::Pinned(inner) => cur = inner,
            _ => return cur,
        }
    }
}

/// Whether a slot admits a `[[wikilink]]` REFERENCE value, wrappers unwrapped.
///
/// This is the gate the value layer uses to decide whether a whole-value
/// wikilink is a `reference` container or the literal string
/// ([[type reference::au-type-system]], "Navigational versus validated": a validated reference
/// needs BOTH that the slot admits one AND that the value is exactly one
/// `[[...]]`).
pub fn slot_admits_reference(shape: &Shape) -> bool {
    match slot_element_shape(shape) {
        // A compound's BRANCHES carry their own suffixes, so `<paper* | String>`
        // is a `Union`, not a `CompoundReference` — the latter is only the
        // whole-compound `<a | b>*` form. A slot admits a reference when ANY
        // branch does, matching how validation routes a wikilink string
        // (`validate.rs`, `branches.iter().any(is_wikilink_handling_shape)`).
        Shape::Union(branches) | Shape::Intersection(branches) => {
            branches.iter().any(slot_admits_reference)
        }
        inner => is_reference_bearing(inner),
    }
}

/// Whether a slot is list-cardinality, so its value yields one container PER
/// ELEMENT ([[type value container::au-type-system]]: "a field's effective value is a list of
/// value containers"). `List` is always the OUTERMOST wrapper — `T*@[]` parses
/// as `List(Pinned(Reference))` and the inner is never a list.
pub fn slot_is_list(shape: &Shape) -> bool {
    matches!(shape, Shape::List { .. })
}

/// How a slot reads NON-record content, or `None` where it reads none.
///
/// The text half of the body-fence content-form question, see
/// [[type-instance body contribution::au-type-system]]. Pairs with [`slot_admits_record`]: a slot
/// answering both is a compound, and the content disambiguates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextForm {
    /// Captured exactly as authored, the multi-line text case. A `String` slot
    /// or the no-type `any`.
    Verbatim,
    /// Parsed as a single scalar, the read the inline `` `[:field] value` ``
    /// marker performs. A non-`String` primitive or an enum.
    ///
    /// Parsing is not optional here: `` `[:count] 42` `` yields a number, so a
    /// fence over `42` must too, or the two carriers produce unequal values for
    /// identical content, fail to collapse, and one authored value counts twice
    /// against cardinality ([[type value container::au-type-system]]).
    Scalar,
}

/// Whether a slot admits an inline RECORD value, wrappers unwrapped.
///
/// The record half of the body-fence content-form question. Mirrors
/// [`slot_admits_reference`]'s any-branch rule for compounds.
///
/// The interpreted top `any` and its `any&` inline branch DO admit a record: a
/// mapping value reads as an inline record ([[type-def shape any::au-type-system]], interpreted).
/// The uninterpreted `opaque` never does, its fence reads verbatim only
/// ([[type-def shape opaque::au-type-system]]), so it falls to the `_` arm.
pub fn slot_admits_record(shape: &Shape) -> bool {
    match slot_element_shape(shape) {
        Shape::Union(branches) | Shape::Intersection(branches) => {
            branches.iter().any(slot_admits_record)
        }
        Shape::Any => true,
        Shape::Record(_) | Shape::InlineOrReference(_) => true,
        // `<a | b>&` defers to the inline-value phase, so its inline branch is a
        // record; the `*` mode is reference-only.
        Shape::CompoundReference { mode, .. } => matches!(mode, RefMode::Inline),
        _ => false,
    }
}

/// How a slot reads non-record content, `None` where it reads none.
///
/// `None` alone does NOT mean reference-only: a plain record slot (`myRec`) also
/// returns `None`, and it does admit inline content, just never text. The
/// reference-only test is the CONJUNCTION, `!slot_admits_record(s) &&
/// slot_text_form(s).is_none()`, which is what both call sites use.
///
/// `Verbatim` wins over `Scalar` in a compound: a `<String | Number>` fence is
/// multi-line-capable, and the narrower scalar read would truncate it.
pub fn slot_text_form(shape: &Shape) -> Option<TextForm> {
    match slot_element_shape(shape) {
        Shape::Primitive(Primitive::String) | Shape::Any | Shape::Opaque => {
            Some(TextForm::Verbatim)
        }
        // A refined String reads verbatim like `String`; a refined Number / Date /
        // DateTime reads as a scalar like its base primitive.
        Shape::Refined {
            base: Primitive::String,
            ..
        } => Some(TextForm::Verbatim),
        Shape::Refined { .. } => Some(TextForm::Scalar),
        Shape::Primitive(_) | Shape::Enum(_) => Some(TextForm::Scalar),
        // `any&` admits an inline value, the interpreted top ([[type-def shape any::au-type-system]]).
        // It also admits a record (`slot_admits_record`), so the two together read
        // a fence by value-shape: a mapping is a record, anything else keeps its
        // text (verbatim here).
        //
        // `any*` is deliberately NOT here: it is reference-ONLY, with no inline
        // form, so a fence at it is a slot mismatch exactly like a fence at `T*`.
        Shape::InlineOrReference(n) if n.as_str() == "any" => Some(TextForm::Verbatim),
        Shape::Union(branches) | Shape::Intersection(branches) => {
            let forms: Vec<TextForm> = branches.iter().filter_map(slot_text_form).collect();
            if forms.contains(&TextForm::Verbatim) {
                Some(TextForm::Verbatim)
            } else {
                forms.first().copied()
            }
        }
        // An INLINE compound (`<a | b>&`) whose branches include the `any`
        // sentinel admits plain text via the interpreted top, so it carries a
        // text form beside its record one and the content disambiguates. Without
        // this the whole compound would force a record read even where `any`
        // makes plain text legal. The `*` mode is reference-only, no text form.
        Shape::CompoundReference {
            mode: RefMode::Inline,
            branches,
            ..
        } if branches.iter().any(|n| n.as_str() == "any") => Some(TextForm::Verbatim),
        _ => None,
    }
}

/// A recognized `Name(...)` constructor value ([[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]).
///
/// The inline value form a NOMINAL brand (scalar / named enum / tuple) carries:
/// `meter(42)`, `quality(reviewed)`, `point(20, 30)`. It closes the gap that a
/// bare scalar has no place to carry its brand, the value-layer parallel of an
/// inline record's `type:` claim.
///
/// The constructor stays a plain string in the parse-layer IR, uninterpreted,
/// the same seam dates, URLs, and wikilinks use — this is a VALUE-layer
/// recognizer, distinct from the shape grammar, applied to an instance value
/// string rather than a `.type.yaml` slot. Validation interprets the extracted
/// `name` and `args` against the slot's resolved brand, where all type-aware
/// scalar interpretation lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constructor {
    /// The brand name before the `(`, a valid [[type-def legal names::au-type-system]] token,
    /// `::repo`-qualifiable (a peer's brand, `icon-role::sdk(save)`).
    pub name: String,
    /// The positional argument strings, split on top-level commas and trimmed.
    /// A nested constructor keeps its own parens, so `rect(point(1,2), point(3,4))`
    /// yields two args, each a nested constructor string, for validation to
    /// recurse into. `Name()` yields an empty list. Splitting is paren-depth and
    /// quote aware, so a comma inside a nested constructor or a quoted string does
    /// not split an argument.
    pub args: Vec<String>,
}

/// The outcome of applying [`recognize_constructor`] to an instance value.
///
/// Tri-state so validation can tell a plain value from a broken one: a bare
/// scalar coerces silently, a well-formed constructor is checked against the
/// slot's brand, and a value that STARTS a `Name(` shape but does not close as
/// one surfaces `malformed-constructor` (a warning). au-grammar emits no
/// diagnostics — it has no source files — so the discrimination is returned, not
/// reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstructorMatch {
    /// Not a constructor: the value does not start `Name(`. Read it as the brand's
    /// bare underlying representation (a plain scalar, a member literal). No
    /// diagnostic.
    NotConstructor,
    /// A well-formed `Name(args)`.
    Constructor(Constructor),
    /// The value starts a `Name(` shape but does not close as a well-formed
    /// constructor: unbalanced parens, or trailing content after the closing `)`.
    /// Validation surfaces `malformed-constructor`.
    Malformed,
}

/// The outcome of applying [`recognize_tuple`] to an instance value: the NAMELESS
/// paren tuple form `(a, b)` an inline tuple slot takes ([[type-def shape tuple::au-type-system]]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TupleMatch {
    /// Not a paren form: the value does not start `(`. A `[...]` bracket is a
    /// LIST, never a tuple, so it never reaches here.
    NotTuple,
    /// A well-formed `(args)`, positional args split at top level (paren / quote /
    /// bracket aware), each an element string for the caller to elaborate.
    Tuple(Vec<String>),
    /// Starts `(` but does not close well-formed: unbalanced, or trailing content.
    Malformed,
}

/// Split the body after an opening `(` into top-level positional args, requiring
/// the matching `)` to be the final non-whitespace character.
///
/// `Some(args)` on a clean close; `None` when malformed (unbalanced, or trailing
/// content after the close). Splitting is paren / square / angle depth and quote
/// aware, so a comma inside a nested tuple, list, union, or quoted string does not
/// split an arg. Shared by [`recognize_constructor`] and [`recognize_tuple`] so the
/// named and nameless forms split identically.
fn parse_paren_args(body: &str) -> Option<Vec<String>> {
    let mut paren: usize = 0;
    let mut square: usize = 0;
    let mut angle: usize = 0;
    let mut in_quote = false;
    let mut args: Vec<String> = Vec::new();
    let mut arg_start = 0usize;
    let mut saw_arg = false;
    for (i, c) in body.char_indices() {
        if in_quote {
            if c == '"' {
                in_quote = false;
            }
            continue;
        }
        let top = paren == 0 && square == 0 && angle == 0;
        match c {
            '"' => in_quote = true,
            '(' => paren += 1,
            '[' => square += 1,
            ']' => square = square.saturating_sub(1),
            '<' => angle += 1,
            '>' => angle = angle.saturating_sub(1),
            ')' if paren > 0 => paren -= 1,
            ')' if top => {
                // The closing paren. Nothing but whitespace may follow it.
                if body[i + c.len_utf8()..].trim().is_empty() {
                    let inner = body[..i].trim();
                    if !inner.is_empty() || saw_arg {
                        args.push(body[arg_start..i].trim().to_string());
                    }
                    return Some(args);
                }
                return None;
            }
            ',' if top => {
                args.push(body[arg_start..i].trim().to_string());
                arg_start = i + c.len_utf8();
                saw_arg = true;
            }
            _ => {}
        }
    }
    // Ran off the end without closing at all-zero depth: unbalanced.
    None
}

/// Recognize the NAMELESS paren tuple form `(a, b)`, the value an inline tuple
/// slot `(A, B)` takes ([[type-def shape tuple::au-type-system]]). A `[...]` bracket is always a
/// LIST value and never reaches here. A named `Name(a, b)` is a brand constructor,
/// see [`recognize_constructor`].
pub fn recognize_tuple(raw: &str) -> TupleMatch {
    let v = raw.trim();
    if !v.starts_with('(') {
        return TupleMatch::NotTuple;
    }
    match parse_paren_args(&v['('.len_utf8()..]) {
        Some(args) => TupleMatch::Tuple(args),
        None => TupleMatch::Malformed,
    }
}

/// Recognize a `Name(...)` constructor in an instance value string.
///
/// "Starts a constructor" means a leading run of [[type-def legal names::au-type-system]] name
/// characters (`::repo`-qualifiable) followed IMMEDIATELY by `(`, no whitespace
/// between. This tight-token rule keeps a prose value like `save (the file)` from
/// reading as a constructor — the space before `(` breaks the token. A value with
/// no such prefix is [`ConstructorMatch::NotConstructor`], left to the bare-value
/// path.
///
/// A value that starts a constructor is well-formed when its parens balance and
/// the first `(`'s matching `)` is the final non-whitespace character, otherwise
/// [`ConstructorMatch::Malformed`]. The name is NOT validated as a legal type name
/// here beyond its leading char — a `::repo` shape or a name the graph lacks is a
/// validation concern, not a lexical one; the recognizer only extracts.
pub fn recognize_constructor(raw: &str) -> ConstructorMatch {
    let v = raw.trim();
    // The name is the leading run of legal name characters; a `::` repo qualifier
    // rides along (its chars are all legal-name plus `:`). The token ends at the
    // first `(`.
    let Some(open) = v.find('(') else {
        return ConstructorMatch::NotConstructor;
    };
    let name = &v[..open];
    if name.is_empty() || !is_constructor_name(name) {
        return ConstructorMatch::NotConstructor;
    }
    // The body is everything after the first `(`. `parse_paren_args` walks it,
    // paren / bracket / quote aware, and closes at the `)` matching the opening
    // paren with nothing but whitespace after it. A `"`-quoted span is opaque; `'`
    // is not a delimiter, so an apostrophe (`O'Brien`) is an ordinary character.
    let body = &v[open + 1..];
    match parse_paren_args(body) {
        Some(args) => ConstructorMatch::Constructor(Constructor {
            name: name.to_string(),
            args,
        }),
        None => ConstructorMatch::Malformed,
    }
}

/// A constructor name is a leading run matching the [[type-def legal names::au-type-system]]
/// type-name token, optionally `::repo`-qualified. Reuses the shape-side
/// validators so the value form and the slot form agree on what a name is.
fn is_constructor_name(s: &str) -> bool {
    match s.split_once("::") {
        None => is_valid_type_name(s),
        Some((base, repo)) => is_valid_type_name(base) && is_valid_repo_name(repo),
    }
}

fn is_reference_bearing(shape: &Shape) -> bool {
    matches!(
        shape,
        Shape::Reference(_)
            | Shape::InlineOrReference(_)
            | Shape::CompoundReference { .. }
            | Shape::DefReference(_)
    )
}

/// Inner side of a `*` reference suffix. [[type-def shape suffixes::au-type-system]]: `*` may attach to a
/// type-def name (incl. dotted sealed-leaf names) or the built-in `file`.
/// It MUST NOT attach to primitives, inline enums, or — once they exist —
/// compounds containing primitives/enums.
fn parse_reference(
    prefix: &str,
    depth: usize,
    ctx: &mut SpanCtx,
) -> Result<Shape, ShapeParseError> {
    let Some(first) = prefix.chars().next() else {
        return Err(syntax_error("'*' suffix has no inner shape"));
    };
    if first == '[' {
        return Err(syntax_error("'*' suffix on inline enum is not allowed"));
    }
    if first == '<' {
        return parse_compound_with_suffix(prefix, RefMode::Star, depth, ctx);
    }
    // [[type-def shape def-ref::au-type-system]]: the `type` keyword in reference position is a
    // typed type-def reference, not a reference to a type-def named "type".
    // `type*` is unconstrained; `type<T>*` carries a def-axis bound.
    if prefix == "type" {
        ctx.record(prefix, ShapeSpanRole::Builtin);
        return Ok(Shape::DefReference(None));
    }
    if let Some(bound) = strip_type_bound(prefix) {
        // The `type` keyword, then the bound names inside `type<...>`.
        ctx.record(&prefix[..4], ShapeSpanRole::Builtin);
        return Ok(Shape::DefReference(Some(parse_def_bound(bound, ctx)?)));
    }
    // Split off an optional `::repo` peer qualifier ([[type-def shape suffixes::au-type-system]]:
    // the qualifier binds to the name, before the suffixes). The base is then
    // validated as a [[type-def legal names::au-type-system]] name; a `::repo` on a built-in is
    // rejected by `qualified_name`.
    let (base, repo) = split_repo_qualifier(prefix)?;
    if base.contains('{') {
        return Err(refinement_bad_shape(
            "a value refinement is inline only and takes no '*' reference suffix",
        ));
    }
    if is_primitive_name(base) {
        return Err(syntax_error(format!(
            "'*' suffix on primitive shape '{}' is not allowed",
            base
        )));
    }
    if base == "opaque" {
        return Err(syntax_error(
            "'opaque*' is not meaningful; opaque is inline-only, use 'any*' for a top-type reference",
        ));
    }
    if base.ends_with('*') || base.ends_with('&') {
        return Err(syntax_error(
            "reference suffix '*' / '&' is mutually exclusive and may appear once",
        ));
    }
    if !is_valid_type_name(base) {
        return Err(syntax_error(format!(
            "'{}' is not a valid type-def name",
            base
        )));
    }
    let name = qualified_name(base, repo)?;
    // `file` / `any` reach here too (unqualified only); classify the keyword vs
    // a user type name by the base. The recorded span covers the whole
    // qualified token.
    ctx.record(prefix, shape_name_role(base));
    Ok(Shape::Reference(name))
}

/// Inner side of an `&` (inline-or-reference) suffix. [[type-def shape suffixes::au-type-system]]: `&` may
/// attach to a type-def name (incl. dotted sealed-leaf names) or to a
/// compound. Like `*`, it MUST NOT attach to primitives, inline enums, or
/// compounds containing them. `file&` is rejected outright ([[type-def shape file::au-type-system]]:
/// `file*` is the only meaningful form for the built-in any-repo-file
/// reference). Single-name input produces `Shape::InlineOrReference`;
/// compound input delegates to `parse_compound_with_suffix`.
fn parse_inline_or_ref(
    prefix: &str,
    depth: usize,
    ctx: &mut SpanCtx,
) -> Result<Shape, ShapeParseError> {
    let Some(first) = prefix.chars().next() else {
        return Err(syntax_error("'&' suffix has no inner shape"));
    };
    if first == '[' {
        return Err(syntax_error("'&' suffix on inline enum is not allowed"));
    }
    if first == '<' {
        return parse_compound_with_suffix(prefix, RefMode::Inline, depth, ctx);
    }
    // [[type-def shape def-ref::au-type-system]]: def-refs are reference-only, there is no
    // `type*`/`type<T>*` inline-or-reference form. Reject `type&` / `type<T>&`.
    if prefix == "type" || strip_type_bound(prefix).is_some() {
        return Err(syntax_error(
            "'type<...>' is a reference-only shape; use 'type*' / 'type<T>*', not '&'",
        ));
    }
    // Split off an optional `::repo` peer qualifier, then validate the base.
    let (base, repo) = split_repo_qualifier(prefix)?;
    if base.contains('{') {
        return Err(refinement_bad_shape(
            "a value refinement is inline only and takes no '&' reference suffix",
        ));
    }
    if is_primitive_name(base) {
        return Err(syntax_error(format!(
            "'&' suffix on primitive shape '{}' is not allowed",
            base
        )));
    }
    if base == "file" {
        return Err(syntax_error(
            "'file&' is not meaningful; use 'file*' for any-repo-file references",
        ));
    }
    if base == "opaque" {
        return Err(syntax_error(
            "'opaque&' is not meaningful; opaque is inline-only, use 'any&' for an inline-or-reference top",
        ));
    }
    if base.ends_with('*') || base.ends_with('&') {
        return Err(syntax_error(
            "reference suffix '*' / '&' is mutually exclusive and may appear once",
        ));
    }
    if !is_valid_type_name(base) {
        return Err(syntax_error(format!(
            "'{}' is not a valid type-def name",
            base
        )));
    }
    let name = qualified_name(base, repo)?;
    ctx.record(prefix, shape_name_role(base));
    Ok(Shape::InlineOrReference(name))
}

/// Parse a bare atom (no `[]` / `*` / `&`): primitive, inline enum, compound,
/// or bare type-def name.
fn parse_atom(s: &str, depth: usize, ctx: &mut SpanCtx) -> Result<Shape, ShapeParseError> {
    let Some(first) = s.chars().next() else {
        return Err(syntax_error("shape is empty"));
    };
    if first == '[' {
        return parse_enum(s, ctx);
    }
    if first == '<' {
        return parse_compound(s, depth, ctx);
    }
    if first == '(' {
        return parse_tuple(s, depth, ctx);
    }
    if !first.is_ascii_alphabetic() {
        return Err(syntax_error(format!(
            "shape starts with invalid character '{}'",
            first
        )));
    }
    // A value refinement `Base{predicate}` on a primitive base ([[type-def field shape::au-type-system]]).
    // The base name is alphabetic, so a `{` here opens a refinement.
    if let Some(brace_pos) = s.find('{') {
        return parse_refinement(s, brace_pos, ctx);
    }
    // [[type-def shape def-ref::au-type-system]]: `type<...>` is reference-only. A bound with no
    // `*` is ill-formed — the `*` is part of the keyword form. (`type<...>*` and
    // `type*` are intercepted in `parse_reference`; bare `type` with no bracket
    // falls through to the record-name path below.)
    if strip_type_bound(s).is_some() {
        return Err(syntax_error(
            "'type<...>' is a reference-only shape; write 'type<...>*'",
        ));
    }
    let primitive = match s {
        "String" => Some(Primitive::String),
        "Number" => Some(Primitive::Number),
        "Boolean" => Some(Primitive::Boolean),
        "Date" => Some(Primitive::Date),
        "DateTime" => Some(Primitive::DateTime),
        "Url" => Some(Primitive::Url),
        _ => None,
    };
    if let Some(p) = primitive {
        ctx.record(s, ShapeSpanRole::Builtin);
        return Ok(Shape::Primitive(p));
    }
    match s {
        // Unlike `file`, bare `any` has an inline form: the no-type slot
        // ([[type-def shape any::au-type-system]]). `any*` / `any&` route through
        // `parse_reference` / `parse_inline_or_ref` before reaching here.
        "any" => {
            ctx.record(s, ShapeSpanRole::Builtin);
            Ok(Shape::Any)
        }
        // The uninterpreted inline slot ([[type-def shape opaque::au-type-system]]). Like `any`
        // it has an inline form; unlike `any` it has NO reference form, so
        // `opaque*` / `opaque&` are rejected in `parse_reference` /
        // `parse_inline_or_ref` before reaching here.
        "opaque" => {
            ctx.record(s, ShapeSpanRole::Builtin);
            Ok(Shape::Opaque)
        }
        "file" => Err(syntax_error(
            "bare 'file' is not meaningful; use 'file*' for any-repo-file references",
        )),
        other => {
            // Bare-name record slot ([[type-def shape record::au-type-system]], e.g. `rationale`), optionally
            // `::repo`-qualified. Value must be an inline YAML map ([[type-def shape record::au-type-system]]).
            // au-core verifies the name exists in the type graph; au-grammar only
            // enforces the [[type-def legal names::au-type-system]] type-name regex here.
            let (base, repo) = split_repo_qualifier(other)?;
            if !is_valid_type_name(base) {
                return Err(syntax_error(format!(
                    "'{}' is not a valid shape — neither primitive, type-def name, enum, nor compound",
                    base
                )));
            }
            let name = qualified_name(base, repo)?;
            ctx.record(other, ShapeSpanRole::TypeName);
            Ok(Shape::Record(name))
        }
    }
}

fn primitive_from_str(s: &str) -> Option<Primitive> {
    match s {
        "String" => Some(Primitive::String),
        "Number" => Some(Primitive::Number),
        "Boolean" => Some(Primitive::Boolean),
        "Date" => Some(Primitive::Date),
        "DateTime" => Some(Primitive::DateTime),
        "Url" => Some(Primitive::Url),
        _ => None,
    }
}

/// The ordered primitives, the ones a comparison predicate applies to.
fn is_ordered_primitive(p: Primitive) -> bool {
    matches!(p, Primitive::Number | Primitive::Date | Primitive::DateTime)
}

/// Parse a value refinement `Base{predicate}` ([[type-def field shape::au-type-system]]). `s` is
/// the whole atom, `brace_pos` the index of the opening `{`. The base must be a
/// refinable primitive; the predicate is validated for that base.
fn parse_refinement(
    s: &str,
    brace_pos: usize,
    ctx: &mut SpanCtx,
) -> Result<Shape, ShapeParseError> {
    if !s.ends_with('}') {
        return Err(refinement_bad_shape(
            "refinement '{...}' is missing its closing '}'",
        ));
    }
    let base_str = s[..brace_pos].trim_end();
    let content = &s[brace_pos + 1..s.len() - 1];
    let Some(base) = primitive_from_str(base_str) else {
        return Err(refinement_bad_shape(format!(
            "refinement '{{...}}' applies to a primitive base; '{}' is not a primitive",
            base_str
        )));
    };
    let refinement = parse_refinement_predicates(base, content)?;
    ctx.record(base_str, ShapeSpanRole::Builtin);
    Ok(Shape::Refined { base, refinement })
}

/// Parse the `&`-joined predicate list inside a refinement's braces into a
/// [`Refinement`], validating each predicate against `base`.
fn parse_refinement_predicates(
    base: Primitive,
    content: &str,
) -> Result<Refinement, ShapeParseError> {
    let mut r = Refinement::default();
    for tok in split_predicates(content)? {
        let t = tok.trim();
        if t.is_empty() {
            return Err(refinement_bad_shape(
                "refinement has an empty predicate (check for leading, trailing, or double '&')",
            ));
        }
        classify_predicate(base, t, &mut r)?;
    }
    if r.lower.is_none() && r.upper.is_none() && !r.integer && r.pattern.is_none() {
        return Err(refinement_bad_shape("refinement '{}' has no predicate"));
    }
    Ok(r)
}

/// Classify one predicate token and fold it into `r`. Rejects a predicate that
/// does not apply to `base`, and a second predicate of the same kind.
fn classify_predicate(base: Primitive, t: &str, r: &mut Refinement) -> Result<(), ShapeParseError> {
    // A regex predicate: `/pattern/`, String base only.
    if t.starts_with('/') {
        if base != Primitive::String {
            return Err(refinement_bad_shape(format!(
                "a regex predicate applies to a 'String' base, not '{}'",
                base.as_str()
            )));
        }
        if t.len() < 2 || !t.ends_with('/') {
            return Err(refinement_bad_shape(
                "regex predicate is missing its closing '/'",
            ));
        }
        if r.pattern.is_some() {
            return Err(refinement_bad_shape(
                "refinement has more than one regex predicate",
            ));
        }
        r.pattern = Some(t[1..t.len() - 1].to_string());
        return Ok(());
    }
    // The `integer` predicate: Number base only.
    if t == "integer" {
        if base != Primitive::Number {
            return Err(refinement_bad_shape(format!(
                "the 'integer' predicate applies to a 'Number' base, not '{}'",
                base.as_str()
            )));
        }
        if r.integer {
            return Err(refinement_bad_shape(
                "refinement has more than one 'integer' predicate",
            ));
        }
        r.integer = true;
        return Ok(());
    }
    // A comparison predicate: `>=` / `>` / `<=` / `<` then a literal, ordered base.
    let (is_lower, inclusive, lit) = if let Some(rest) = t.strip_prefix(">=") {
        (true, true, rest)
    } else if let Some(rest) = t.strip_prefix('>') {
        (true, false, rest)
    } else if let Some(rest) = t.strip_prefix("<=") {
        (false, true, rest)
    } else if let Some(rest) = t.strip_prefix('<') {
        (false, false, rest)
    } else {
        return Err(refinement_bad_shape(format!(
            "'{}' is not a recognized refinement predicate (expected a comparison, 'integer', or a /regex/)",
            t
        )));
    };
    if !is_ordered_primitive(base) {
        return Err(refinement_bad_shape(format!(
            "a comparison predicate applies to an ordered primitive (Number, Date, DateTime), not '{}'",
            base.as_str()
        )));
    }
    let value = normalize_comparison_literal(base, lit.trim())?;
    let bound = Bound { value, inclusive };
    if is_lower {
        if r.lower.is_some() {
            return Err(refinement_bad_shape(
                "refinement has more than one lower bound",
            ));
        }
        r.lower = Some(bound);
    } else {
        if r.upper.is_some() {
            return Err(refinement_bad_shape(
                "refinement has more than one upper bound",
            ));
        }
        r.upper = Some(bound);
    }
    Ok(())
}

/// Split a refinement's brace content on each top-level `&`, keeping a `/.../`
/// regex region intact (its `&` is a literal pattern byte, `\/` does not close
/// it). Returns the raw predicate slices; the caller trims and classifies each.
fn split_predicates(content: &str) -> Result<Vec<&str>, ShapeParseError> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut in_regex = false;
    let mut escaped = false;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_regex {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'/' {
                in_regex = false;
            }
        } else if b == b'/' {
            in_regex = true;
        } else if b == b'&' {
            out.push(&content[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    if in_regex {
        return Err(refinement_bad_shape(
            "regex predicate is missing its closing '/'",
        ));
    }
    out.push(&content[start..]);
    Ok(out)
}

/// Validate and canonicalize a comparison literal for `base`. A `Number` literal
/// is normalized to its shortest exact decimal; a `Date` / `DateTime` literal is
/// stored as written (non-empty, no interior whitespace), au-core checks its
/// calendar validity.
fn normalize_comparison_literal(base: Primitive, lit: &str) -> Result<String, ShapeParseError> {
    if base == Primitive::Number {
        return normalize_number_literal(lit);
    }
    // Date / DateTime: light shape check only; calendar validity is au-core's.
    if lit.is_empty() || lit.chars().any(|c| c.is_whitespace()) {
        return Err(refinement_bad_shape(format!(
            "'{}' is not a valid {} literal",
            lit,
            base.as_str()
        )));
    }
    Ok(lit.to_string())
}

/// Canonicalize a decimal number literal: strip a leading `-` on zero, strip
/// leading integer zeros, strip trailing fractional zeros, drop an empty
/// fraction. So `0.0` / `-0` / `00` all render `0`, and `1.50` renders `1.5`.
/// No scientific notation. Rejects a malformed literal.
fn normalize_number_literal(s: &str) -> Result<String, ShapeParseError> {
    let malformed = || refinement_bad_shape(format!("'{}' is not a valid number literal", s));
    let neg = s.starts_with('-');
    let body = s.strip_prefix('-').unwrap_or(s);
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(malformed());
    }
    if let Some(f) = frac_part {
        if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
            return Err(malformed());
        }
    }
    let int_norm = {
        let stripped = int_part.trim_start_matches('0');
        if stripped.is_empty() {
            "0"
        } else {
            stripped
        }
    };
    let frac_norm = frac_part
        .map(|f| f.trim_end_matches('0'))
        .filter(|f| !f.is_empty());
    let is_zero = int_norm == "0" && frac_norm.is_none();
    let mut out = String::new();
    if neg && !is_zero {
        out.push('-');
    }
    out.push_str(int_norm);
    if let Some(f) = frac_norm {
        out.push('.');
        out.push_str(f);
    }
    Ok(out)
}

/// Parse a compound expression `<X | Y>` or `<X & Y>` into a `Shape::Union`
/// or `Shape::Intersection`. [[type-def shape compound::au-type-system]]. Branches preserve source order
/// per [[type-def fields collision - auto-unify and qualified field::au-type-system]] (token equality is structural over the `Vec`).
///
/// Caller (`parse_atom`) has verified the input starts with `<`. The closing
/// `>` is checked here. One operator type per compound: `<X | Y & Z>` is a
/// `shape-syntax-error` (use nesting like `<<X | Y> & Z>` instead). Empty
/// (`<>`) and single-branch (`<X>`) compounds are rejected — use the bare
/// form `X` for one branch.
///
/// Suffixes on compounds (`<...>*` / `<...>&` / `<...>[]`) are handled
/// upstream: `[]` strips at `parse_with_list_suffix` and recurses into
/// this function transparently; `*` and `&` route through
/// `parse_reference` / `parse_inline_or_ref` which surface a "compound
/// suffix not yet implemented" diagnostic distinct from "compound parsing
/// not yet implemented" (deferred to a later action).
/// Parse a tuple shape `(A, B, ...)` into `Shape::Tuple`
/// ([[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]):
/// a fixed-arity positional product, every element required, each a full shape.
/// Elements split on top-level commas (paren / angle / square depth aware,
/// refinements skipped); `()` (no elements) is an error. A trailing suffix on
/// the tuple (`(A, B)[]` a list of tuples) is stripped upstream before the
/// tuple is reached, exactly like a compound.
fn parse_tuple(raw: &str, depth: usize, ctx: &mut SpanCtx) -> Result<Shape, ShapeParseError> {
    let inner = raw
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| syntax_error("tuple shape is missing its closing ')'"))?;
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        return Err(syntax_error(
            "tuple shape '()' has no elements; a tuple is a fixed-arity product",
        ));
    }
    if let Err(reason) = bracket_balance(trimmed) {
        return Err(syntax_error(format!(
            "tuple shape's inner content has unbalanced brackets ({reason})"
        )));
    }
    let parts = split_top_level(trimmed, ',');
    let mut elements = Vec::with_capacity(parts.len());
    for part in &parts {
        let p = part.trim();
        if p.is_empty() {
            return Err(syntax_error(
                "tuple shape has an empty element (check for leading, trailing, or double ',')",
            ));
        }
        elements.push(parse_with_list_suffix(p, depth + 1, ctx)?);
    }
    Ok(Shape::Tuple(elements))
}

fn parse_compound(raw: &str, depth: usize, ctx: &mut SpanCtx) -> Result<Shape, ShapeParseError> {
    let inner = raw
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .ok_or_else(|| syntax_error("compound shape is missing its closing '>'"))?;

    let trimmed = inner.trim();
    if trimmed.is_empty() {
        return Err(syntax_error("compound shape '<>' has no branches"));
    }
    // After stripping outer `<...>`, the inner can still be imbalanced
    // even though the whole input was balanced — e.g. `<>X<>` is
    // globally balanced but its inner `>X<` goes negative. Likely the
    // user wrote sibling compounds without nesting them.
    if let Err(reason) = bracket_balance(trimmed) {
        return Err(syntax_error(format!(
            "compound shape's inner content has unbalanced brackets ({reason}); nest deeper compounds explicitly like '<<X | Y> & Z>'"
        )));
    }

    let has_pipe = has_top_level_op(trimmed, '|');
    let has_amp = has_top_level_op(trimmed, '&');
    let (op_char, is_union) = match (has_pipe, has_amp) {
        (true, true) => {
            return Err(syntax_error(
                "compound shape mixes '|' and '&' operators; one operator type per compound (use nesting like '<<X | Y> & Z>' for mixes)",
            ));
        }
        (true, false) => ('|', true),
        (false, true) => ('&', false),
        (false, false) => {
            return Err(syntax_error(
                "compound shape has only one branch; use the bare form 'X' or add another branch with '|' or '&'",
            ));
        }
    };

    let parts = split_top_level(trimmed, op_char);
    let mut branches = Vec::with_capacity(parts.len());
    for part in &parts {
        let p = part.trim();
        if p.is_empty() {
            return Err(syntax_error(format!(
                "compound shape has an empty branch (check for leading, trailing, or double '{}')",
                op_char
            )));
        }
        branches.push(parse_with_list_suffix(p, depth + 1, ctx)?);
    }

    Ok(if is_union {
        Shape::Union(branches)
    } else {
        Shape::Intersection(branches)
    })
}

/// Parse a compound with a `*` or `&` suffix into a `Shape::CompoundReference`.
/// [[type-def shape suffixes::au-type-system]]: the suffix attaches to a compound whose operands are all
/// type-def names — primitives and inline enums in the compound are
/// rejected here, not at `parse_compound` level (a bare `<String | Number>`
/// without suffix is a perfectly fine union slot).
///
/// Caller has already verified `raw` starts with `<`. We delegate the
/// bracket / operator / branch-trim work to `parse_compound`, then collapse
/// the result into the flatter `CompoundReference` form (branches become
/// `Vec<String>` of names — non-name branches surface as `shape-syntax-error`).
fn parse_compound_with_suffix(
    raw: &str,
    mode: RefMode,
    depth: usize,
    ctx: &mut SpanCtx,
) -> Result<Shape, ShapeParseError> {
    // parse_compound records each operand's name span during its descent;
    // collapsing to CompoundReference.branches below adds no new spans.
    let inner = parse_compound(raw, depth, ctx)?;
    let (op, branch_shapes) = match inner {
        Shape::Union(b) => (CompoundRefOp::Union, b),
        Shape::Intersection(b) => (CompoundRefOp::Intersection, b),
        _ => unreachable!("parse_compound only produces Union or Intersection"),
    };
    let mut branches = Vec::with_capacity(branch_shapes.len());
    for branch in branch_shapes {
        // [[type-def shape suffixes::au-type-system]]: compound operands are all type-def names. Whether
        // each operand was written bare (`rationale`), as a typed
        // reference (`rationale*`), or as inline-or-reference
        // (`rationale&`), they all reduce to the same name in the
        // CompoundReference.branches list. The suffix attaches once at
        // the compound level.
        let name = match branch {
            Shape::Reference(name) | Shape::Record(name) | Shape::InlineOrReference(name) => name,
            Shape::Primitive(p) => {
                return Err(syntax_error(format!(
                    "'{}' suffix on compound containing primitive shape '{}' is not allowed",
                    mode.suffix_char(),
                    p
                )));
            }
            Shape::Enum(_) => {
                return Err(syntax_error(format!(
                    "'{}' suffix on compound containing inline enum is not allowed",
                    mode.suffix_char()
                )));
            }
            other => {
                return Err(syntax_error(format!(
                    "'{}' suffix on compound containing non-reference shape '{}' is not allowed",
                    mode.suffix_char(),
                    other
                )));
            }
        };
        branches.push(name);
    }
    Ok(Shape::CompoundReference { mode, op, branches })
}

/// If `s` is the keyword-bracketed `type<...>` form, return the inner bound
/// (between the brackets). Used by `parse_reference` (the `type<T>*` case),
/// `parse_inline_or_ref` (reject `type<T>&`), and `parse_atom` (reject a
/// suffix-less `type<T>`). [[type-def shape def-ref::au-type-system]].
fn strip_type_bound(s: &str) -> Option<&str> {
    s.strip_prefix("type<")?.strip_suffix('>')
}

/// Parse the bound inside a `type<...>*` def-reference into a `DefBound`.
/// [[type-def shape def-ref::au-type-system]]. The bound is a single type-def name or a compound
/// of names — the same compound-of-names that `*` / `&` accept, restricted to
/// the def axis. Mixed `|`/`&`, empty branches, and non-name operands are
/// rejected, mirroring `parse_compound` / `parse_compound_with_suffix`. Nested
/// shapes are not permitted, the ceilings are bare type-def names.
fn parse_def_bound(inner: &str, ctx: &mut SpanCtx) -> Result<DefBound, ShapeParseError> {
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        return Err(syntax_error(
            "'type<>' has an empty bound; use 'type*' for any type-def",
        ));
    }
    let has_pipe = has_top_level_op(trimmed, '|');
    let has_amp = has_top_level_op(trimmed, '&');
    let op_char = match (has_pipe, has_amp) {
        (true, true) => {
            return Err(syntax_error(
                "'type<...>' bound mixes '|' and '&' operators; one operator type per bound",
            ));
        }
        (true, false) => '|',
        (false, true) => '&',
        (false, false) => {
            let (base, repo) = split_repo_qualifier(trimmed)?;
            if !is_valid_type_name(base) {
                return Err(syntax_error(format!(
                    "'{}' is not a valid type-def name in a 'type<...>*' bound",
                    base
                )));
            }
            let name = qualified_name(base, repo)?;
            ctx.record(trimmed, ShapeSpanRole::TypeName);
            return Ok(DefBound::Single(name));
        }
    };
    let op = if has_pipe {
        CompoundRefOp::Union
    } else {
        CompoundRefOp::Intersection
    };
    let parts = split_top_level(trimmed, op_char);
    let mut branches = Vec::with_capacity(parts.len());
    for part in &parts {
        let p = part.trim();
        if p.is_empty() {
            return Err(syntax_error(format!(
                "'type<...>' bound has an empty branch (check for leading, trailing, or double '{}')",
                op_char
            )));
        }
        let (base, repo) = split_repo_qualifier(p)?;
        if !is_valid_type_name(base) {
            return Err(syntax_error(format!(
                "'{}' is not a valid type-def name in a 'type<...>*' bound",
                base
            )));
        }
        ctx.record(p, ShapeSpanRole::TypeName);
        branches.push(qualified_name(base, repo)?);
    }
    Ok(DefBound::Compound { op, branches })
}

/// Verify that `s` has balanced `<...>` and `[...]` brackets. "Balanced"
/// means depth never goes negative reading left-to-right and ends at
/// zero. On imbalance, returns a short reason string suitable for
/// inclusion in a diagnostic message.
///
/// Used as an early pre-check by `parse_shape` and as a post-strip check
/// by `parse_compound`, so accidental typos like `<X>Y>` or `<>X<>`
/// produce a focused "unbalanced brackets" diagnostic instead of
/// surfacing as misleading downstream errors ("single branch", "shape
/// uses a deferred feature").
fn bracket_balance(s: &str) -> Result<(), &'static str> {
    let bytes = s.as_bytes();
    let mut angle: i32 = 0;
    let mut square: i32 = 0;
    let mut paren: i32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // A value refinement `{...}` is opaque to the compound / list
            // structure: its comparison `<` `>` and regex `[` `]` `<` `>` `|`
            // must not count as brackets. Skip the whole region; an
            // unterminated one is left for `parse_refinement` to report.
            b'{' => {
                if let Some(end) = refinement_span_end(bytes, i) {
                    i = end;
                    continue;
                }
                return Ok(());
            }
            b'<' => angle += 1,
            b'>' => {
                angle -= 1;
                if angle < 0 {
                    return Err("unbalanced '>'");
                }
            }
            b'[' => square += 1,
            b']' => {
                square -= 1;
                if square < 0 {
                    return Err("unbalanced ']'");
                }
            }
            // Tuple `(...)` brackets, balanced like the others.
            b'(' => paren += 1,
            b')' => {
                paren -= 1;
                if paren < 0 {
                    return Err("unbalanced ')'");
                }
            }
            _ => {}
        }
        i += 1;
    }
    if paren != 0 {
        return Err("missing closing ')'");
    }
    if angle != 0 {
        return Err("missing closing '>'");
    }
    if square != 0 {
        return Err("missing closing ']'");
    }
    Ok(())
}

/// If a value refinement `{...}` opens at byte `i`, return the index just past
/// its closing `}`. A `/.../` regex region inside is opaque — its `{` `}` `<`
/// `>` `[` `]` `|` `&` are literal pattern bytes, and `\/` does not close the
/// regex. Returns `None` if `bytes[i]` is not `{`, or the brace is unterminated.
/// This is the one delimiter-aware primitive every structural scanner uses to
/// treat a refinement as a single opaque unit.
fn refinement_span_end(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes.get(i) != Some(&b'{') {
        return None;
    }
    let mut j = i + 1;
    let mut in_regex = false;
    let mut escaped = false;
    while j < bytes.len() {
        let b = bytes[j];
        if in_regex {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'/' {
                in_regex = false;
            }
        } else {
            match b {
                b'/' => in_regex = true,
                b'}' => return Some(j + 1),
                _ => {}
            }
        }
        j += 1;
    }
    None
}

/// Returns true if `s` contains `op` outside of any nested `<...>` or
/// `[...]` brackets. Used by `parse_compound` to detect the operator at
/// the compound's own depth without descending into nested compounds or
/// inline enums.
/// Whether the `op` char at byte index `i` in `s` is a compound OPERATOR
/// rather than a branch's inline-or-reference `&` suffix ([[type-def shape suffixes::au-type-system]]).
///
/// `|` is never a suffix, so any top-level `|` is the union operator. `&` is
/// ambiguous: it is both the intersection operator (`<a & b>`) and the
/// inline-or-reference suffix on a branch (the `a&` in `<a& | b>`). The two are
/// told apart structurally, a suffix `&` TERMINATES a branch:
/// - a base name ends on its left (alphanumeric / `_` / `-` / `.`), and
/// - no new operand begins on its right (the next char is not a base-start
///   letter, nor a nested-compound `<`).
///
/// So `<a& | b>` and `<a& & b>` read the `a&` as a suffix, while the operator
/// forms `<a & b>` (space on the left) and `<a&b>` (a base-start `b` on the
/// right) stay intersections. Every spaced operator has whitespace on its left,
/// so no existing compound changes meaning, only inputs rejected today move.
fn is_operator_at(s: &str, i: usize, op: char) -> bool {
    if op != '&' {
        return true;
    }
    let bytes = s.as_bytes();
    let prev_is_base_ender = i
        .checked_sub(1)
        .map(|j| bytes[j])
        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    let next_is_base_starter = bytes
        .get(i + 1)
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'<');
    // Suffix iff a base ends on the left AND no operand begins on the right.
    !(prev_is_base_ender && !next_is_base_starter)
}

fn has_top_level_op(s: &str, op: char) -> bool {
    let bytes = s.as_bytes();
    let op_b = op as u8;
    let mut angle: i32 = 0;
    let mut square: i32 = 0;
    let mut paren: i32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                if let Some(end) = refinement_span_end(bytes, i) {
                    i = end;
                    continue;
                }
            }
            b'<' => angle += 1,
            b'>' => angle -= 1,
            b'[' => square += 1,
            b']' => square -= 1,
            b'(' => paren += 1,
            b')' => paren -= 1,
            b if b == op_b
                && angle == 0
                && square == 0
                && paren == 0
                && is_operator_at(s, i, op) =>
            {
                return true
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Split `s` on every operator occurrence of `op` outside any nested `<...>` or
/// `[...]` brackets. Returns at least one slice (possibly empty). A `&` that is
/// a branch suffix rather than the intersection operator is not a split point,
/// see [`is_operator_at`].
fn split_top_level(s: &str, op: char) -> Vec<&str> {
    let bytes = s.as_bytes();
    let op_b = op as u8;
    let mut out = Vec::new();
    let mut angle: i32 = 0;
    let mut square: i32 = 0;
    let mut paren: i32 = 0;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                if let Some(end) = refinement_span_end(bytes, i) {
                    i = end;
                    continue;
                }
            }
            b'<' => angle += 1,
            b'>' => angle -= 1,
            b'[' => square += 1,
            b']' => square -= 1,
            b'(' => paren += 1,
            b')' => paren -= 1,
            b if b == op_b
                && angle == 0
                && square == 0
                && paren == 0
                && is_operator_at(s, i, op) =>
            {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

/// Whether a name is a reserved built-in primitive keyword (`String`, `Number`,
/// `Boolean`, `Date`, `DateTime`, `Url`). These double as always-available
/// primitive constructors, so a `Name(...)` whose name is one is a reserved
/// primitive constructor, see
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
pub fn is_primitive_name(s: &str) -> bool {
    matches!(
        s,
        "String" | "Number" | "Boolean" | "Date" | "DateTime" | "Url"
    )
}

/// Mirrors the [[type-def legal names::au-type-system]] type-name regex without pulling in a regex dep:
/// `^[A-Za-z][A-Za-z0-9_-]*(\.[A-Za-z][A-Za-z0-9_-]*)*$`. au-core enforces
/// the same rule on type-def names; this keeps slot-side names consistent.
fn is_valid_type_name(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    for segment in s.split('.') {
        let mut chars = segment.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !first.is_ascii_alphabetic() {
            return false;
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return false;
        }
    }
    true
}

/// A repo name in a `::repo` qualifier is a single-segment identifier, matching
/// au-references' `is_valid_repo_name`: a leading letter, then letters, digits,
/// `_`, `-`. No dots (a repo is not a sealed-leaf name).
fn is_valid_repo_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The built-in shape keywords. A `::repo` qualifier names a peer's type-def,
/// so it can never attach to one of these — they are engine-side sentinels,
/// not peer-owned types.
fn is_reserved_builtin(s: &str) -> bool {
    is_primitive_name(s) || s == "file" || s == "any" || s == "opaque" || s == "type"
}

/// Split a type-name token into its base and optional `::repo` qualifier.
///
/// The `::` is unambiguous in shape position: `:` is not a shape operator and
/// is illegal inside a [[type-def legal names::au-type-system]] name, so a `::` can only be the
/// repo qualifier. Splits on the first `::`, so a stray second `::` lands in the
/// repo half and fails `is_valid_repo_name`. Rejects an empty base, an empty
/// repo (`foo::`, the shape sibling of `wikilink-empty-repo`), and a malformed
/// repo name. Does NOT validate the base — the caller does, since an unqualified
/// base may be a built-in (`file`, a primitive). A `::repo` on a built-in is
/// rejected by the caller via `is_reserved_builtin`.
fn split_repo_qualifier(token: &str) -> Result<(&str, Option<&str>), ShapeParseError> {
    match token.split_once("::") {
        None => Ok((token, None)),
        Some((base, repo)) => {
            if base.is_empty() {
                return Err(syntax_error(format!(
                    "'{}' has an empty type name before '::'",
                    token
                )));
            }
            if repo.is_empty() {
                return Err(syntax_error(format!(
                    "'{}' has an empty repo scope after '::'; write 'name::repo' or drop the '::'",
                    token
                )));
            }
            if !is_valid_repo_name(repo) {
                return Err(syntax_error(format!(
                    "'{}' is not a valid repo name in a '::repo' qualifier",
                    repo
                )));
            }
            Ok((base, Some(repo)))
        }
    }
}

/// Build a `QualifiedName` from a validated base and optional repo, rejecting a
/// `::repo` on a built-in keyword. Shared by the three single-name reference
/// parsers and the def-bound parser. `base` is assumed already validated as a
/// [[type-def legal names::au-type-system]] name by the caller; the built-in check happens here
/// because only a qualified built-in is illegal (bare `file` / `any` are fine).
fn qualified_name(base: &str, repo: Option<&str>) -> Result<QualifiedName, ShapeParseError> {
    if repo.is_some() && is_reserved_builtin(base) {
        return Err(syntax_error(format!(
            "built-in shape '{}' cannot carry a '::repo' qualifier; '::repo' names a peer type-def",
            base
        )));
    }
    Ok(QualifiedName {
        base: base.to_string(),
        repo: repo.map(str::to_string),
    })
}

fn syntax_error(message: impl Into<String>) -> ShapeParseError {
    ShapeParseError {
        code: SHAPE_SYNTAX_ERROR,
        severity: Severity::Error,
        message: message.into(),
    }
}

fn refinement_bad_shape(message: impl Into<String>) -> ShapeParseError {
    ShapeParseError {
        code: REFINEMENT_BAD_SHAPE,
        severity: Severity::Error,
        message: message.into(),
    }
}

fn cardinality_bad_shape(message: impl Into<String>) -> ShapeParseError {
    ShapeParseError {
        code: CARDINALITY_BAD_SHAPE,
        severity: Severity::Error,
        message: message.into(),
    }
}

/// Parse an inline closed enum `[v1, v2, ...]`.
///
/// Caller has already trimmed `raw` and verified it starts with `[`. V1
/// alphabet for literals is `[A-Za-z0-9_.-]+`; whitespace inside a literal
/// and quoted literals are rejected. Trailing comma is accepted to match
/// YAML flow-sequence ergonomics.
fn parse_enum(raw: &str, ctx: &mut SpanCtx) -> Result<Shape, ShapeParseError> {
    let inner = raw
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| ShapeParseError {
            code: SHAPE_SYNTAX_ERROR,
            severity: Severity::Error,
            message: "enum shape is missing its closing ']'".into(),
        })?;

    if inner.trim().is_empty() {
        return Err(ShapeParseError {
            code: SHAPE_SYNTAX_ERROR,
            severity: Severity::Error,
            message: "enum shape must have at least one literal".into(),
        });
    }

    let parts: Vec<&str> = inner.split(',').collect();
    let last_index = parts.len() - 1;
    let mut literals: Vec<String> = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        let token = part.trim();
        if token.is_empty() {
            // Trailing comma after at least one literal is fine.
            if i == last_index && !literals.is_empty() {
                continue;
            }
            return Err(ShapeParseError {
                code: SHAPE_SYNTAX_ERROR,
                severity: Severity::Error,
                message: "enum has an empty literal (check for leading or stray commas)".into(),
            });
        }
        if !is_valid_enum_literal(token) {
            return Err(ShapeParseError {
                code: SHAPE_SYNTAX_ERROR,
                severity: Severity::Error,
                message: format!("enum literal '{}' violates the type-name regex", token),
            });
        }
        ctx.record(token, ShapeSpanRole::EnumMember);
        literals.push(token.to_string());
    }

    Ok(Shape::Enum(literals))
}

/// [[type-def shape enum::au-type-system]]: enum elements follow the [[type-def legal names::au-type-system]] type-name regex. Same rule
/// as `is_valid_type_name` — segmented `^[A-Za-z][A-Za-z0-9_-]*(\.[A-Za-z]...)*$`.
/// Excludes shape-grammar characters (`+`, `*`, etc.) and YAML-numeric forms
/// (`1.5`, `42`) so enum values stay YAML-string-shaped at instance sites.
fn is_valid_enum_literal(s: &str) -> bool {
    is_valid_type_name(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `qualify_bare` re-qualifies bare user names to the given repo and leaves
    /// built-ins, enums, and already-qualified names alone, recursing through
    /// list / compound / pinned wrappers. Compared as ASTs (parse the expected
    /// source) to stay robust to Display formatting.
    #[test]
    fn qualify_bare_re_qualifies_only_bare_user_names() {
        let cases = [
            // bare user names, at every name-bearing position
            ("foo*", "foo::base*"),
            ("foo", "foo::base"),
            ("foo&", "foo::base&"),
            ("foo*[]", "foo::base*[]"),
            ("foo[+]", "foo::base[+]"),
            ("<a | b>*", "<a::base | b::base>*"),
            ("<a & b>&", "<a::base & b::base>&"),
            ("<a | b>", "<a::base | b::base>"),
            ("foo*@", "foo::base*@"),
            // built-ins are never re-qualified
            ("file*", "file*"),
            ("any*", "any*"),
            ("any", "any"),
            ("String", "String"),
            ("[low, high]", "[low, high]"),
            ("<foo* | file*>", "<foo::base* | file*>"),
            // already-qualified names are left as authored
            ("foo::other*", "foo::other*"),
            ("<a::other | b>*", "<a::other | b::base>*"),
        ];
        for (src, want) in cases {
            let got = parse_shape(src)
                .unwrap_or_else(|e| panic!("parse {src:?}: {e:?}"))
                .qualify_bare("base");
            let expected =
                parse_shape(want).unwrap_or_else(|e| panic!("parse want {want:?}: {e:?}"));
            assert_eq!(got, expected, "qualify_bare({src:?})");
        }
    }

    #[test]
    fn qualify_bare_re_qualifies_a_def_reference_bound() {
        // A `type<T>*` ceiling is a type-name position, re-qualified like any other
        // (built-ins and already-qualified ceilings stay put).
        for (src, want) in [
            ("type<mcp.tool>*", "type<mcp.tool::base>*"),
            ("type<a | b>*", "type<a::base | b::base>*"),
            ("type<a::other>*", "type<a::other>*"),
            ("type*", "type*"),
        ] {
            let got = parse_shape(src).unwrap().qualify_bare("base");
            let expected = parse_shape(want).unwrap();
            assert_eq!(got, expected, "qualify_bare({src:?})");
        }
    }

    // Per-primitive parse coverage lives in `primitive_as_str_round_trips`
    // below — iterating the full set is strictly stronger than five
    // hand-written tests.

    #[test]
    fn whitespace_around_primitive_is_trimmed() {
        // YAML may hand us shape strings with surrounding whitespace.
        assert_eq!(
            parse_shape("  String  "),
            Ok(Shape::Primitive(Primitive::String))
        );
    }

    #[test]
    fn lowercase_primitive_name_is_not_a_primitive() {
        // `string` (lowercase) is a valid type-def name per the [[type-def legal names::au-type-system]]
        // regex, so it parses as a bare-name record slot — not the
        // `String` primitive (capital S only). au-core verifies the name
        // exists in the type graph at load time.
        assert_eq!(parse_shape("string"), Ok(Shape::Record("string".into())));
    }

    #[test]
    fn bare_record_slot_parses() {
        // Bare type-def names parse as `Shape::Record` ([[type-def shape record::au-type-system]]):
        // the slot expects an inline YAML map. Distinct from
        // `Shape::Reference` (the `name*` form, wikilink-only).
        assert_eq!(
            parse_shape("rationale"),
            Ok(Shape::Record("rationale".into()))
        );
        assert_eq!(
            parse_shape("decision.decided"),
            Ok(Shape::Record("decision.decided".into()))
        );
    }

    #[test]
    fn bare_record_with_invalid_name_is_syntax_error() {
        // The [[type-def legal names::au-type-system]] type-name regex still gates bare names — typos and
        // junk text don't slip through as records.
        let err = parse_shape("not a name").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn inline_or_reference_bare_name_parses() {
        // Bare `name&` is the [[type-def shape suffixes::au-type-system]] inline-or-reference single-name
        // form: value may be an inline map or a wikilink.
        assert_eq!(
            parse_shape("rationale&"),
            Ok(Shape::InlineOrReference("rationale".into()))
        );
    }

    #[test]
    fn inline_or_reference_dotted_name_parses() {
        assert_eq!(
            parse_shape("decision.decided&"),
            Ok(Shape::InlineOrReference("decision.decided".into()))
        );
    }

    #[test]
    fn simple_reference_parses() {
        assert_eq!(
            parse_shape("rationale*"),
            Ok(Shape::Reference("rationale".into()))
        );
    }

    #[test]
    fn dotted_reference_parses() {
        // Sealed-leaf names stay valid as reference targets.
        assert_eq!(
            parse_shape("decision.decided*"),
            Ok(Shape::Reference("decision.decided".into()))
        );
    }

    #[test]
    fn file_reference_parses() {
        // Built-in `file*` is the any-repo-file reference ([[type-def shape file::au-type-system]]).
        // au-grammar treats it as an ordinary `Shape::Reference`; au-core
        // recognizes the name and skips the closure check at validation.
        assert_eq!(parse_shape("file*"), Ok(Shape::Reference("file".into())));
    }

    #[test]
    fn any_forms_parse() {
        // Bare `any` is the no-type inline slot ([[type-def shape any::au-type-system]]), unlike
        // bare `file` which errors. `any[]` falls out of the list machinery.
        assert_eq!(parse_shape("any"), Ok(Shape::Any));
        assert_eq!(
            parse_shape("any[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Any),
                min: 0,
                max: None,
            })
        );
        assert_eq!(
            parse_shape("any[+]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Any),
                min: 1,
                max: None,
            })
        );
        // The reference forms route through the suffix parsers and land as
        // `"any"`-named references, like `file*`. au-core treats the name as
        // a built-in target with no closure check.
        assert_eq!(parse_shape("any*"), Ok(Shape::Reference("any".into())));
        assert_eq!(
            parse_shape("any&"),
            Ok(Shape::InlineOrReference("any".into()))
        );
        assert_eq!(
            parse_shape("any*[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Reference("any".into())),
                min: 0,
                max: None,
            })
        );
    }

    #[test]
    fn any_roundtrips_through_display() {
        for src in ["any", "any[]", "any[+]", "any*", "any&"] {
            let shape = parse_shape(src).unwrap();
            assert_eq!(shape.to_string(), src, "Display mismatch for {src}");
        }
    }

    #[test]
    fn opaque_forms_parse() {
        // [[type-def shape opaque::au-type-system]]: the uninterpreted inline slot.
        assert_eq!(parse_shape("opaque"), Ok(Shape::Opaque));
        assert_eq!(
            parse_shape("opaque[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Opaque),
                min: 0,
                max: None,
            })
        );
        assert_eq!(
            parse_shape("opaque[+]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Opaque),
                min: 1,
                max: None,
            })
        );
    }

    #[test]
    fn opaque_has_no_reference_form() {
        // Inline-only: a reference is interpreted by definition, so the top-type
        // reference is `any*` / `any&`, never `opaque*` / `opaque&`.
        assert!(parse_shape("opaque*").is_err());
        assert!(parse_shape("opaque&").is_err());
        assert!(parse_shape("opaque*@").is_err());
    }

    #[test]
    fn opaque_roundtrips_through_display() {
        for src in ["opaque", "opaque[]", "opaque[+]"] {
            let shape = parse_shape(src).unwrap();
            assert_eq!(shape.to_string(), src, "Display mismatch for {src}");
        }
    }

    #[test]
    fn opaque_fence_reads_verbatim_never_a_record() {
        // The uninterpreted slot reads a fence verbatim and never as a record,
        // the distinction from interpreted `any`. [[type-instance body contribution::au-type-system]].
        assert_eq!(slot_text_form(&Shape::Opaque), Some(TextForm::Verbatim));
        assert!(!slot_admits_record(&Shape::Opaque));
    }

    #[test]
    fn opaque_is_a_reserved_builtin() {
        assert!(is_reserved_builtin("opaque"));
    }

    /// The body-fence content-form predicates, [[type-instance body contribution::au-type-system]].
    /// `slot_admits_record` and `slot_text_form` together classify every slot.
    #[test]
    fn fence_content_form_predicates_classify_every_slot() {
        // (shape, admits_record, text_form)
        let cases: &[(&str, bool, Option<TextForm>)] = &[
            // A record-bearing slot reads a record and nothing else.
            ("myRec", true, None),
            ("myRec&", true, None),
            // Text slots.
            ("String", false, Some(TextForm::Verbatim)),
            // The uninterpreted `opaque` reads verbatim and never a record.
            ("opaque", false, Some(TextForm::Verbatim)),
            // The interpreted top `any` reads by value-shape: it admits a record
            // (a mapping) AND carries a text form (anything else).
            ("any", true, Some(TextForm::Verbatim)),
            ("any&", true, Some(TextForm::Verbatim)),
            ("Number", false, Some(TextForm::Scalar)),
            ("Date", false, Some(TextForm::Scalar)),
            ("[low, high]", false, Some(TextForm::Scalar)),
            // Reference-only slots admit no inline content at all, so a fence at
            // one is a slot mismatch rather than any kind of read.
            ("myRec*", false, None),
            ("file*", false, None),
            ("any*", false, None),
            ("type<myRec>*", false, None),
            // Compounds: the content disambiguates when both are admitted.
            ("<String | myRec>", true, Some(TextForm::Verbatim)),
            ("<Number | myRec>", true, Some(TextForm::Scalar)),
            // Verbatim beats Scalar, so a `<String | Number>` fence stays
            // multi-line-capable instead of being truncated by the scalar read.
            ("<String | Number>", false, Some(TextForm::Verbatim)),
            // Wrappers unwrap, exactly as `slot_admits_reference` does.
            ("String[]", false, Some(TextForm::Verbatim)),
            ("myRec[]", true, None),
            ("myRec*@", false, None),
        ];
        for (src, want_record, want_text) in cases {
            let shape = parse_shape(src).unwrap_or_else(|e| panic!("{src} failed to parse: {e:?}"));
            assert_eq!(
                slot_admits_record(&shape),
                *want_record,
                "slot_admits_record mismatch for {src}"
            );
            assert_eq!(
                slot_text_form(&shape),
                *want_text,
                "slot_text_form mismatch for {src}"
            );
        }
    }

    /// `any&` admits inline content whose branch is OPAQUE, so it reads as text,
    /// never as a record. `any*` is reference-only and reads as neither.
    #[test]
    fn any_reads_by_value_shape_opaque_reads_verbatim() {
        // The interpreted top `any&` admits a record AND carries a text form, so
        // its fence reads by value-shape ([[type-def shape any::au-type-system]]).
        let inline_or_ref = parse_shape("any&").unwrap();
        assert!(slot_admits_record(&inline_or_ref));
        assert_eq!(slot_text_form(&inline_or_ref), Some(TextForm::Verbatim));

        // The uninterpreted `opaque` admits NO record, only verbatim text
        // ([[type-def shape opaque::au-type-system]]).
        let opaque = parse_shape("opaque").unwrap();
        assert!(!slot_admits_record(&opaque));
        assert_eq!(slot_text_form(&opaque), Some(TextForm::Verbatim));

        let reference = parse_shape("any*").unwrap();
        assert!(!slot_admits_record(&reference));
        assert_eq!(
            slot_text_form(&reference),
            None,
            "`any*` is reference-only, so it admits no fence content"
        );

        // An ordinary user `&` type is a record slot too.
        let user = parse_shape("notAny&").unwrap();
        assert!(slot_admits_record(&user));
    }

    #[test]
    fn def_reference_forms_parse() {
        // [[type-def shape def-ref::au-type-system]]: the `type` keyword in reference position.
        assert_eq!(parse_shape("type*"), Ok(Shape::DefReference(None)));
        assert_eq!(
            parse_shape("type<mcp.tool>*"),
            Ok(Shape::DefReference(Some(DefBound::Single(
                "mcp.tool".into()
            ))))
        );
        assert_eq!(
            parse_shape("type<mcp.tool | mcp.resource>*"),
            Ok(Shape::DefReference(Some(DefBound::Compound {
                op: CompoundRefOp::Union,
                branches: vec!["mcp.tool".into(), "mcp.resource".into()],
            })))
        );
        assert_eq!(
            parse_shape("type<a & b>*"),
            Ok(Shape::DefReference(Some(DefBound::Compound {
                op: CompoundRefOp::Intersection,
                branches: vec!["a".into(), "b".into()],
            })))
        );
    }

    #[test]
    fn def_reference_list_parses() {
        // `[]` / `[+]` attach outside the keyword form, like `T*[]`.
        assert_eq!(
            parse_shape("type<mcp.tool>*[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::DefReference(Some(DefBound::Single(
                    "mcp.tool".into()
                )))),
                min: 0,
                max: None,
            })
        );
    }

    #[test]
    fn def_reference_roundtrips_through_display() {
        for src in [
            "type*",
            "type<mcp.tool>*",
            "type<mcp.tool | mcp.resource>*",
            "type<a & b>*",
        ] {
            let shape = parse_shape(src).unwrap();
            assert_eq!(shape.to_string(), src, "Display mismatch for {src}");
        }
    }

    // ----- `@` pin-enforcement postfix ([[type-def shape suffixes::au-type-system]]) -----

    #[test]
    fn pin_postfix_wraps_a_reference_shape() {
        assert_eq!(
            parse_shape("file*@"),
            Ok(Shape::Pinned(Box::new(Shape::Reference("file".into()))))
        );
        assert_eq!(
            parse_shape("myType*@"),
            Ok(Shape::Pinned(Box::new(Shape::Reference("myType".into()))))
        );
        // `T&@` is ILLEGAL: `@` requires a `*` reference, a `&` inline branch
        // cannot pin. The intent is the union `< T& | T*@ >`.
        assert!(
            parse_shape("myType&@").is_err(),
            "T&@ must be rejected, @ requires a '*' reference"
        );
        assert_eq!(
            parse_shape("type<mcp.tool>*@"),
            Ok(Shape::Pinned(Box::new(Shape::DefReference(Some(
                DefBound::Single("mcp.tool".into())
            )))))
        );
        assert_eq!(
            parse_shape("<a | b>*@"),
            Ok(Shape::Pinned(Box::new(Shape::CompoundReference {
                mode: RefMode::Star,
                op: CompoundRefOp::Union,
                branches: vec!["a".into(), "b".into()],
            })))
        );
    }

    #[test]
    fn pin_postfix_is_inside_the_list_suffix() {
        // Order is reference suffix, then `@`, then list suffix: `T*@[]` is a
        // list of pinned references, `List(Pinned(Reference))`.
        assert_eq!(
            parse_shape("myType*@[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Pinned(Box::new(Shape::Reference("myType".into())))),
                min: 0,
                max: None,
            })
        );
    }

    #[test]
    fn pin_postfix_roundtrips_through_display() {
        for src in ["file*@", "myType*@", "type<mcp.tool>*@", "myType*@[]"] {
            let shape = parse_shape(src).unwrap();
            assert_eq!(shape.to_string(), src, "Display mismatch for {src}");
        }
    }

    #[test]
    fn pin_postfix_on_a_non_reference_is_an_error() {
        // `@` attaches to a reference only, never a primitive, enum, or bare record.
        for bad in ["String@", "[a, b]@", "myType@", "any@"] {
            assert!(
                parse_shape(bad).is_err(),
                "{bad} should reject a pin on a non-reference"
            );
        }
    }

    #[test]
    fn double_pin_is_an_error() {
        assert!(parse_shape("myType*@@").is_err());
    }

    #[test]
    fn def_reference_reference_only_forms_are_errors() {
        // [[type-def shape def-ref::au-type-system]]: def-refs are reference-only — `*` is part of the
        // keyword form. A suffix-less `type<T>` and the inline `type<T>&` /
        // `type&` forms are rejected.
        for raw in ["type<mcp.tool>", "type<mcp.tool>&", "type&"] {
            let err = parse_shape(raw).unwrap_err();
            assert_eq!(err.code, SHAPE_SYNTAX_ERROR, "'{}' should be rejected", raw);
        }
    }

    #[test]
    fn def_reference_malformed_bounds_are_errors() {
        for raw in [
            "type<>*",          // empty bound
            "type<a | b & c>*", // mixed operators
            "type<a*>*",        // operand is not a bare name
            "type< >*",         // whitespace-only bound
        ] {
            let err = parse_shape(raw).unwrap_err();
            assert_eq!(err.code, SHAPE_SYNTAX_ERROR, "'{}' should be rejected", raw);
        }
    }

    #[test]
    fn whitespace_around_reference_is_trimmed() {
        assert_eq!(
            parse_shape("  rationale*  "),
            Ok(Shape::Reference("rationale".into()))
        );
        assert_eq!(
            parse_shape("rationale *"),
            Ok(Shape::Reference("rationale".into()))
        );
    }

    #[test]
    fn primitive_with_star_is_syntax_error() {
        for prim in ["String", "Number", "Boolean", "Date", "DateTime", "Url"] {
            let raw = format!("{}*", prim);
            let err = parse_shape(&raw).unwrap_err();
            assert_eq!(
                err.code, SHAPE_SYNTAX_ERROR,
                "'{}' should be rejected, not deferred",
                raw
            );
            assert!(
                err.message.contains(prim),
                "message should name the primitive: {}",
                err.message
            );
        }
    }

    #[test]
    fn enum_with_star_is_syntax_error() {
        let err = parse_shape("[a, b]*").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn compound_reference_star_parses() {
        // `<X | Y>*` is a typed reference whose target's `type:` closure
        // satisfies any of `branches` (Union case). [[type-def shape suffixes::au-type-system]] preferred
        // form is `<rationale | thesis>*` (bare names) but bare records
        // are still NYI today — the explicit-`*`-per-branch form is the
        // current workaround. The CompoundReference flattens both forms
        // to the same AST, so when bare records land they parse to the
        // same shape.
        assert_eq!(
            parse_shape("<rationale* | thesis*>*"),
            Ok(Shape::CompoundReference {
                mode: RefMode::Star,
                op: CompoundRefOp::Union,
                branches: vec!["rationale".into(), "thesis".into()],
            })
        );
    }

    #[test]
    fn compound_reference_amp_parses() {
        // `<X & Y>&` is an inline-or-reference whose closure satisfies all
        // of `branches` (Intersection case). Validation semantics for the
        // inline branch land in a later phase; parsing succeeds today.
        assert_eq!(
            parse_shape("<rationale* & maturity*>&"),
            Ok(Shape::CompoundReference {
                mode: RefMode::Inline,
                op: CompoundRefOp::Intersection,
                branches: vec!["rationale".into(), "maturity".into()],
            })
        );
    }

    #[test]
    fn bare_name_branch_in_compound_reference_parses() {
        // Spec-preferred `<rationale | thesis>*` form. Bare-name
        // branches parse as `Shape::Record`; `parse_compound_with_suffix`
        // collapses them to the same flat `branches: Vec<String>` as the
        // explicit-`*`-per-branch workaround `<rationale* | thesis*>*`.
        assert_eq!(
            parse_shape("<rationale | thesis>*"),
            Ok(Shape::CompoundReference {
                mode: RefMode::Star,
                op: CompoundRefOp::Union,
                branches: vec!["rationale".into(), "thesis".into()],
            })
        );
        // Both forms produce identical AST.
        assert_eq!(
            parse_shape("<rationale | thesis>*"),
            parse_shape("<rationale* | thesis*>*")
        );
    }

    #[test]
    fn list_of_compound_reference_parses() {
        // `<X | Y>*[]` — outermost `[]`, inner is a CompoundReference.
        assert_eq!(
            parse_shape("<rationale* | thesis*>*[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::CompoundReference {
                    mode: RefMode::Star,
                    op: CompoundRefOp::Union,
                    branches: vec!["rationale".into(), "thesis".into()],
                }),
                min: 0,
                max: None
            })
        );
    }

    #[test]
    fn compound_reference_star_with_primitive_branch_is_syntax_error() {
        // `<String | rationale*>*` rejects: [[type-def shape suffixes::au-type-system]] forbids `*` on a
        // compound containing a primitive shape.
        let err = parse_shape("<String | rationale*>*").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
        assert!(
            err.message.contains("primitive"),
            "message should name the rejected branch kind: {}",
            err.message
        );
    }

    #[test]
    fn compound_reference_amp_with_enum_branch_is_syntax_error() {
        let err = parse_shape("<rationale* | [low, high]>&").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
        assert!(
            err.message.contains("inline enum"),
            "message should name the rejected branch kind: {}",
            err.message
        );
    }

    #[test]
    fn compound_reference_with_list_branch_is_syntax_error() {
        // `<a*[] | b*>*` — the first branch is a list, not a bare-name
        // reference. Per [[type-def shape suffixes::au-type-system]] ("operands are all type-def names")
        // this is rejected.
        let err = parse_shape("<a*[] | b*>*").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn compound_reference_display_round_trips() {
        let cases: &[&str] = &[
            "<rationale* | thesis*>*",
            "<rationale* & maturity*>&",
            "<a* | b* | c*>*",
            "<rationale* | thesis*>*[]",
        ];
        for src in cases {
            let shape = parse_shape(src).unwrap();
            let rendered = shape.to_string();
            let reparsed = parse_shape(&rendered).unwrap();
            assert_eq!(shape, reparsed, "round-trip failed for {src:?}");
        }
    }

    #[test]
    fn bare_file_is_syntax_error() {
        // [[type-def shape file::au-type-system]]: `file` is meaningful only with `*`.
        let err = parse_shape("file").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn file_amp_is_syntax_error() {
        let err = parse_shape("file&").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn primitive_amp_is_syntax_error() {
        let err = parse_shape("String&").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn enum_amp_is_syntax_error() {
        let err = parse_shape("[a, b]&").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn name_amp_parses_as_inline_or_reference() {
        // `name&` is the inline-or-reference single-name slot
        // ([[type-def shape suffixes::au-type-system]]). Distinct from `name*` (wikilink-only) and
        // bare `name` (inline-only).
        assert_eq!(
            parse_shape("rationale&"),
            Ok(Shape::InlineOrReference("rationale".into()))
        );
    }

    #[test]
    fn double_star_is_syntax_error() {
        // `*` and `&` are mutually exclusive and single.
        let err = parse_shape("rationale**").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn deeply_repeated_list_suffix_is_rejected_without_overflow() {
        // A `.type.yaml` field shape is untrusted. A long run of trailing
        // `[]` once recursed once per suffix — `String` + 100k `[]` overflowed
        // the stack and aborted the process. The depth cap turns it into a
        // bounded `shape-syntax-error` instead.
        let raw = format!("String{}", "[]".repeat(10_000));
        let err = parse_shape(&raw).unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
        assert!(
            err.message.contains("too deep"),
            "expected a depth diagnostic, got: {}",
            err.message
        );
    }

    #[test]
    fn deeply_nested_compound_is_rejected_without_overflow() {
        // The other recursion source: nested `<...>` compounds. Each level
        // wraps the previous in a fresh union branch, so without the cap the
        // parser recurses once per level and overflows. The cap rejects it.
        let mut raw = String::from("a*");
        for _ in 0..10_000 {
            raw = format!("<{} | a*>", raw);
        }
        let err = parse_shape(&raw).unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
        assert!(
            err.message.contains("too deep"),
            "expected a depth diagnostic, got: {}",
            err.message
        );
    }

    #[test]
    fn realistic_nesting_still_parses() {
        // The cap sits far above any legitimate shape — ordinary nesting is
        // unaffected.
        assert!(parse_shape("<<a* | b*> & c*>[]").is_ok());
        assert!(parse_shape("String[][]").is_ok());
    }

    #[test]
    fn mixed_suffixes_is_syntax_error() {
        let err = parse_shape("rationale*&").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn star_with_no_inner_is_syntax_error() {
        let err = parse_shape("*").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn reference_with_invalid_name_is_syntax_error() {
        // `$` is outside the [[type-def legal names::au-type-system]] type-name regex.
        let err = parse_shape("foo$bar*").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn reference_with_leading_dot_is_syntax_error() {
        // Dotted segments must each start with a letter ([[type-def legal names::au-type-system]]).
        let err = parse_shape(".foo*").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn reference_token_equality() {
        let a = parse_shape("note*").unwrap();
        let b = parse_shape("note*").unwrap();
        let c = parse_shape("decision*").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        // References are not the same shape as the bare-name record they
        // would resolve to (records are a deferred surface).
        assert_ne!(a, Shape::Primitive(Primitive::String));
    }

    #[test]
    fn inline_enum_parses_to_shape_enum() {
        assert_eq!(
            parse_shape("[low, high]"),
            Ok(Shape::Enum(vec!["low".into(), "high".into()]))
        );
    }

    #[test]
    fn enum_single_literal() {
        assert_eq!(parse_shape("[only]"), Ok(Shape::Enum(vec!["only".into()])));
    }

    #[test]
    fn enum_trims_whitespace_around_literals() {
        // The token between commas is whitespace-trimmed; `[a,b]` and
        // `[ a , b ]` produce the same `Shape::Enum`.
        assert_eq!(
            parse_shape("[ low ,  high ]"),
            Ok(Shape::Enum(vec!["low".into(), "high".into()]))
        );
    }

    #[test]
    fn enum_trailing_comma_accepted() {
        assert_eq!(
            parse_shape("[low, high,]"),
            Ok(Shape::Enum(vec!["low".into(), "high".into()]))
        );
    }

    #[test]
    fn enum_preserves_declaration_order() {
        // Order matters for token-equality ([[type-def fields collision - auto-unify and qualified field::au-type-system]]). `[a, b, c]` and
        // `[c, b, a]` are different shapes.
        assert_eq!(
            parse_shape("[c, b, a]"),
            Ok(Shape::Enum(vec!["c".into(), "b".into(), "a".into()]))
        );
    }

    #[test]
    fn enum_token_equality_is_order_sensitive() {
        let ab = parse_shape("[a, b]").unwrap();
        let ab2 = parse_shape("[a, b]").unwrap();
        let ab_no_space = parse_shape("[a,b]").unwrap();
        let ba = parse_shape("[b, a]").unwrap();
        let abc = parse_shape("[a, b, c]").unwrap();
        assert_eq!(ab, ab2);
        assert_eq!(ab, ab_no_space, "whitespace around commas is non-semantic");
        assert_ne!(ab, ba, "reordered literals are not token-equal");
        assert_ne!(ab, abc, "different lengths are not token-equal");
    }

    #[test]
    fn enum_empty_is_syntax_error() {
        let err = parse_shape("[]").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn enum_whitespace_only_is_syntax_error() {
        let err = parse_shape("[   ]").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn enum_leading_comma_is_syntax_error() {
        let err = parse_shape("[, low]").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn enum_double_comma_is_syntax_error() {
        let err = parse_shape("[low, , high]").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn enum_no_separator_is_syntax_error() {
        // Whitespace inside a literal isn't allowed in V1.
        let err = parse_shape("[low high]").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn enum_quoted_literal_is_syntax_error() {
        let err = parse_shape("[low, \"high\"]").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn enum_missing_close_bracket_is_syntax_error() {
        let err = parse_shape("[low, high").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn enum_literal_must_follow_type_name_regex() {
        // [[type-def shape enum::au-type-system]] / [[type-def legal names::au-type-system]]: enum elements share the type-name regex. Forms
        // that used to slip through the looser per-char allowlist now
        // fail at parse: leading digit, leading/trailing dot, doubled
        // dot, numeric-looking literals.
        for bad in ["[1abc]", "[.abc]", "[abc.]", "[a..b]", "[1.5]", "[42]"] {
            let err = parse_shape(bad).unwrap_err();
            assert_eq!(
                err.code, SHAPE_SYNTAX_ERROR,
                "{bad} should be rejected under tightened enum-literal regex"
            );
        }
    }

    #[test]
    fn enum_with_dotted_literal() {
        // `.` is allowed in literals (matches the type-name regex's allowed set,
        // so naming an enum value after a sealed-leaf form like `decision.pending`
        // works as expected).
        assert_eq!(
            parse_shape("[decision.pending, decision.decided]"),
            Ok(Shape::Enum(vec![
                "decision.pending".into(),
                "decision.decided".into(),
            ]))
        );
    }

    #[test]
    fn union_of_two_references_parses() {
        // Branches today must be already-parseable shapes — primitives,
        // enums, references (`name*`), or lists. Bare records (`rationale`
        // without `*`) are still NYI; the compound parser inherits that.
        assert_eq!(
            parse_shape("<rationale* | thesis*>"),
            Ok(Shape::Union(vec![
                Shape::Reference("rationale".into()),
                Shape::Reference("thesis".into()),
            ]))
        );
    }

    #[test]
    fn union_of_two_primitives_parses() {
        assert_eq!(
            parse_shape("<String | Number>"),
            Ok(Shape::Union(vec![
                Shape::Primitive(Primitive::String),
                Shape::Primitive(Primitive::Number),
            ]))
        );
    }

    #[test]
    fn intersection_of_two_references_parses() {
        assert_eq!(
            parse_shape("<rationale* & maturity*>"),
            Ok(Shape::Intersection(vec![
                Shape::Reference("rationale".into()),
                Shape::Reference("maturity".into()),
            ]))
        );
    }

    #[test]
    fn ternary_union_parses_as_three_branches() {
        // `<A | B | C>` is one Union with three entries (chained, not
        // nested) — left-associative folding falls out of split_top_level.
        assert_eq!(
            parse_shape("<rationale* | thesis* | source.url*>"),
            Ok(Shape::Union(vec![
                Shape::Reference("rationale".into()),
                Shape::Reference("thesis".into()),
                Shape::Reference("source.url".into()),
            ]))
        );
    }

    #[test]
    fn nested_compound_parses() {
        // The compound parser respects bracket depth so nested compounds
        // pass through the recursive parse_with_list_suffix call cleanly.
        assert_eq!(
            parse_shape("<<a* | b*> & c*>"),
            Ok(Shape::Intersection(vec![
                Shape::Union(vec![
                    Shape::Reference("a".into()),
                    Shape::Reference("b".into()),
                ]),
                Shape::Reference("c".into()),
            ]))
        );
    }

    #[test]
    fn compound_with_inline_enum_branch_parses() {
        // Splitting respects `[`/`]` depth, so a `|` inside an enum literal
        // (which can't legally contain `|` anyway, but is structurally
        // possible to mishandle) does not fragment the branches.
        assert_eq!(
            parse_shape("<[low, high] | String>"),
            Ok(Shape::Union(vec![
                Shape::Enum(vec!["low".into(), "high".into()]),
                Shape::Primitive(Primitive::String),
            ]))
        );
    }

    #[test]
    fn compound_with_whitespace_is_trimmed() {
        // Inner whitespace and per-branch whitespace both trim away.
        assert_eq!(
            parse_shape("<  rationale*  |  thesis*  >"),
            Ok(Shape::Union(vec![
                Shape::Reference("rationale".into()),
                Shape::Reference("thesis".into()),
            ]))
        );
    }

    #[test]
    fn list_of_compound_parses() {
        // `<X | Y>[]` falls out transparently: parse_with_list_suffix strips
        // outermost `[]` and recurses into parse_compound for the inner.
        assert_eq!(
            parse_shape("<rationale* | thesis*>[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Union(vec![
                    Shape::Reference("rationale".into()),
                    Shape::Reference("thesis".into()),
                ])),
                min: 0,
                max: None
            })
        );
    }

    #[test]
    fn mixed_operators_in_compound_is_syntax_error() {
        // One operator type per compound. Use nesting (`<<X | Y> & Z>`) for
        // mixes.
        let err = parse_shape("<a* | b* & c*>").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
        assert!(
            err.message.contains("mixes") || err.message.contains("one operator"),
            "message should explain the rule: {}",
            err.message
        );
    }

    // --- Per-branch inline-or-reference `&` suffix inside a compound. ---
    // [[type-def shape suffixes::au-type-system]]: a compound branch may carry its own `&`
    // (inline-or-reference) suffix, the parity of the already-legal per-branch
    // `*` (`<String | T*>`). The intersection operator `&` and the branch suffix
    // `&` are told apart structurally (see `is_operator_at`), so a suffixed
    // branch is reachable in both a union and an intersection.

    #[test]
    fn union_with_leading_inline_or_ref_branch_parses() {
        // The `&` on `a` is a suffix (a boundary follows), the `|` the operator.
        assert_eq!(
            parse_shape("<a& | b>"),
            Ok(Shape::Union(vec![
                Shape::InlineOrReference("a".into()),
                Shape::Record("b".into()),
            ]))
        );
    }

    #[test]
    fn union_with_trailing_inline_or_ref_branch_parses() {
        // The suffix `&` sits at the very end of the trimmed inner (no char
        // follows), still a suffix, not a dangling operator.
        assert_eq!(
            parse_shape("<a | b&>"),
            Ok(Shape::Union(vec![
                Shape::Record("a".into()),
                Shape::InlineOrReference("b".into()),
            ]))
        );
    }

    #[test]
    fn union_of_a_plain_and_a_pinned_reference_parses() {
        // The mixed-slot escape hatch, `< T* | T*@ >`: a bare union whose
        // branches carry DIFFERENT suffixes, one plain `*`, one pinned `*@`. Each
        // branch parses with its own suffix, so a pinned value can route to the
        // `*@` branch and an unpinned one to the plain `*` branch.
        assert_eq!(
            parse_shape("<a* | a*@>"),
            Ok(Shape::Union(vec![
                Shape::Reference("a".into()),
                Shape::Pinned(Box::new(Shape::Reference("a".into()))),
            ]))
        );
    }

    #[test]
    fn union_with_both_branches_inline_or_ref_parses() {
        assert_eq!(
            parse_shape("<a& | b&>"),
            Ok(Shape::Union(vec![
                Shape::InlineOrReference("a".into()),
                Shape::InlineOrReference("b".into()),
            ]))
        );
    }

    #[test]
    fn intersection_with_inline_or_ref_branch_parses() {
        // First `&` is a suffix on `a` (boundary follows), second is the
        // space-delimited intersection operator.
        assert_eq!(
            parse_shape("<a& & b>"),
            Ok(Shape::Intersection(vec![
                Shape::InlineOrReference("a".into()),
                Shape::Record("b".into()),
            ]))
        );
    }

    #[test]
    fn intersection_with_both_branches_inline_or_ref_parses() {
        assert_eq!(
            parse_shape("<a& & b&>"),
            Ok(Shape::Intersection(vec![
                Shape::InlineOrReference("a".into()),
                Shape::InlineOrReference("b".into()),
            ]))
        );
    }

    #[test]
    fn compound_branch_mixes_amp_and_star_suffixes() {
        // Per-branch suffixes are independent: `a&` inline-or-ref, `b*` ref.
        assert_eq!(
            parse_shape("<a& | b*>"),
            Ok(Shape::Union(vec![
                Shape::InlineOrReference("a".into()),
                Shape::Reference("b".into()),
            ]))
        );
    }

    #[test]
    fn qualified_inline_or_ref_branch_keeps_its_repo() {
        // The `&` follows the `::repo` qualifier; `note::base` is the branch
        // base, the `&` its suffix, and the repo survives.
        assert_eq!(
            parse_shape("<note::base& | b>"),
            Ok(Shape::Union(vec![
                Shape::InlineOrReference("note::base".into()),
                Shape::Record("b".into()),
            ]))
        );
    }

    #[test]
    fn dotted_name_inline_or_ref_branch_parses() {
        // A `.` is a valid mid-name char, so it is a base-ender before the `&`.
        assert_eq!(
            parse_shape("<a.b& | c>"),
            Ok(Shape::Union(vec![
                Shape::InlineOrReference("a.b".into()),
                Shape::Record("c".into()),
            ]))
        );
    }

    #[test]
    fn ternary_compound_with_inline_or_ref_branches_parses() {
        assert_eq!(
            parse_shape("<a& | b | c&>"),
            Ok(Shape::Union(vec![
                Shape::InlineOrReference("a".into()),
                Shape::Record("b".into()),
                Shape::InlineOrReference("c".into()),
            ]))
        );
    }

    // --- Regression guards: the intersection operator `&` is unchanged. ---

    #[test]
    fn spaced_intersection_operator_of_records_unchanged() {
        assert_eq!(
            parse_shape("<a & b>"),
            Ok(Shape::Intersection(vec![
                Shape::Record("a".into()),
                Shape::Record("b".into()),
            ]))
        );
    }

    #[test]
    fn spaceless_intersection_operator_still_parses() {
        // `<a&b>`: a base-start `b` follows the `&`, so it is the intersection
        // operator, not a suffix. The two-sided rule preserves this form.
        assert_eq!(
            parse_shape("<a&b>"),
            Ok(Shape::Intersection(vec![
                Shape::Record("a".into()),
                Shape::Record("b".into()),
            ]))
        );
    }

    #[test]
    fn whole_compound_amp_suffix_unchanged() {
        // `<a | b>&` is the whole-compound inline-or-ref suffix
        // (CompoundReference), handled upstream and distinct from a per-branch
        // `&`. It roundtrips through the `*`-per-branch Display form.
        assert_eq!(
            parse_shape("<a | b>&").map(|s| s.to_string()),
            Ok("<a* | b*>&".to_string())
        );
    }

    #[test]
    fn double_intersection_operator_is_empty_branch_error() {
        // Two space-delimited `&` operators in a row leave an empty middle
        // branch. Distinct from `<a& & b>` (suffix then operator).
        let err = parse_shape("<a & & b>").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
        assert!(
            err.message.contains("empty branch"),
            "message should name the empty branch: {}",
            err.message
        );
    }

    #[test]
    fn empty_compound_is_syntax_error() {
        let err = parse_shape("<>").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn single_branch_compound_is_syntax_error() {
        // `<X>` has no operator at depth 0 — bare form should be used instead.
        let err = parse_shape("<rationale*>").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn leading_separator_in_compound_is_syntax_error() {
        let err = parse_shape("<| rationale*>").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn trailing_separator_in_compound_is_syntax_error() {
        let err = parse_shape("<rationale* |>").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn double_separator_in_compound_is_syntax_error() {
        let err = parse_shape("<a* | | b*>").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn compound_missing_close_bracket_is_syntax_error() {
        // Use a non-suffix-ending input so the dispatch reaches parse_atom
        // → parse_compound → strip_suffix('>') failure. (Inputs ending in
        // `*` / `&` route through ref-suffix stripping first, which surfaces
        // the deferred compound-suffix message — that's tested separately.)
        let err = parse_shape("<String | Number").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn extra_close_bracket_at_end_reports_unbalanced() {
        // `<X>Y>` — without the bracket-balance pre-check, this would
        // strip ONE outer `>` and reach the compound parser with `X>Y`,
        // which has no top-level operator — surfacing a misleading
        // "single branch" message. The pre-check produces a focused
        // diagnostic instead.
        let err = parse_shape("<a*>Y>").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
        assert!(
            err.message.contains("unbalanced"),
            "expected 'unbalanced' diagnostic, got: {}",
            err.message
        );
    }

    #[test]
    fn globally_balanced_but_inner_imbalanced_reports_unbalanced() {
        // `<>X<>` — globally balanced (2 opens, 2 closes), but stripping
        // the outermost `<...>` leaves `>X<` which goes depth-negative.
        // Likely a typo for nested compounds. The post-strip check in
        // parse_compound catches this; without it the user got "single
        // branch".
        let err = parse_shape("<>X<>").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
        assert!(
            err.message.contains("unbalanced"),
            "expected 'unbalanced' diagnostic, got: {}",
            err.message
        );
    }

    #[test]
    fn sibling_compounds_without_nesting_reports_unbalanced() {
        // `<a*> | <b*>` — user probably meant `<<a*> | <b*>>` (one outer
        // compound with two compound branches). Whole input is balanced
        // globally, but stripping outer brackets leaves an inner with
        // an unbalanced `>` before the matching `<`. The diagnostic now
        // suggests explicit nesting.
        let err = parse_shape("<a*> | <b*>").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
        assert!(
            err.message.contains("unbalanced"),
            "expected 'unbalanced' diagnostic, got: {}",
            err.message
        );
        assert!(
            err.message.contains("nest"),
            "expected nesting suggestion, got: {}",
            err.message
        );
    }

    #[test]
    fn stray_close_bracket_outside_compound_reports_unbalanced() {
        // `X>` doesn't start with `<` — without the pre-check, parse_atom
        // would fall through to "bare records and compounds are deferred"
        // (NYI), which is wrong-feeling for a typo.
        let err = parse_shape("X>").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
        assert!(
            err.message.contains("unbalanced"),
            "expected 'unbalanced' diagnostic, got: {}",
            err.message
        );
    }

    #[test]
    fn unclosed_inline_enum_reports_unbalanced() {
        // `[a, b` — pre-check catches at top level. Existing behavior
        // (parse_enum's own "missing closing ']'" message) is shadowed
        // by the earlier check; both flow through SHAPE_SYNTAX_ERROR.
        let err = parse_shape("[a, b").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
        assert!(
            err.message.contains("unbalanced") || err.message.contains("missing"),
            "expected 'unbalanced' or 'missing' diagnostic, got: {}",
            err.message
        );
    }

    #[test]
    fn compound_branch_order_is_significant() {
        // [[type-def fields collision - auto-unify and qualified field::au-type-system]]: token equality is structural over the branch Vec, so
        // `<a* | b*>` ≠ `<b* | a*>`.
        let ab = parse_shape("<a* | b*>").unwrap();
        let ba = parse_shape("<b* | a*>").unwrap();
        assert_ne!(ab, ba);
    }

    #[test]
    fn compound_display_round_trips() {
        // Display renders the source form. Parsing the rendered string
        // produces the same Shape — locks the source-form contract.
        let cases: &[&str] = &[
            "<rationale* | thesis*>",
            "<String | Number>",
            "<rationale* & maturity*>",
            "<rationale* | thesis* | source.url*>",
            "<<a* | b*> & c*>",
            "<rationale* | thesis*>[]",
            "<[low, high] | String>",
        ];
        for src in cases {
            let shape = parse_shape(src).unwrap();
            let rendered = shape.to_string();
            let reparsed = parse_shape(&rendered).unwrap();
            assert_eq!(shape, reparsed, "round-trip failed for {src:?}");
        }
    }

    #[test]
    fn bare_record_branch_in_compound_parses() {
        // Bare-name records parse via `parse_atom` to `Shape::Record`;
        // a Union of bare records is the spec-canonical record-slot
        // union form ([[type-def shape record::au-type-system]] case 3 entry point).
        assert_eq!(
            parse_shape("<rationale | thesis>"),
            Ok(Shape::Union(vec![
                Shape::Record("rationale".into()),
                Shape::Record("thesis".into()),
            ]))
        );
    }

    #[test]
    fn list_of_string_parses() {
        assert_eq!(
            parse_shape("String[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Primitive(Primitive::String)),
                min: 0,
                max: None
            })
        );
    }

    fn list(inner: Shape, min: u32, max: Option<u32>) -> Shape {
        Shape::List {
            inner: Box::new(inner),
            min,
            max,
        }
    }

    #[test]
    fn range_cardinality_forms_parse() {
        let s = || Shape::Primitive(Primitive::String);
        assert_eq!(parse_shape("String[]"), Ok(list(s(), 0, None)));
        assert_eq!(parse_shape("String[+]"), Ok(list(s(), 1, None)));
        assert_eq!(parse_shape("String[3]"), Ok(list(s(), 3, Some(3))));
        assert_eq!(parse_shape("String[3..]"), Ok(list(s(), 3, None)));
        assert_eq!(parse_shape("String[..5]"), Ok(list(s(), 0, Some(5))));
        assert_eq!(parse_shape("String[2..5]"), Ok(list(s(), 2, Some(5))));
        assert_eq!(parse_shape("String[0..0]"), Ok(list(s(), 0, Some(0))));
    }

    #[test]
    fn range_cardinality_sugar_is_equivalent() {
        // The sugar forms desugar to the same AST as their explicit range.
        assert_eq!(parse_shape("String[]"), parse_shape("String[0..]"));
        assert_eq!(parse_shape("String[+]"), parse_shape("String[1..]"));
        assert_eq!(parse_shape("String[3]"), parse_shape("String[3..3]"));
        assert_eq!(parse_shape("String[..5]"), parse_shape("String[0..5]"));
    }

    #[test]
    fn range_cardinality_whitespace_is_trimmed() {
        let s = Shape::Primitive(Primitive::String);
        assert_eq!(parse_shape("String[ 2 .. 5 ]"), Ok(list(s, 2, Some(5))));
    }

    #[test]
    fn range_cardinality_malformed_is_rejected() {
        assert!(parse_shape("String[5..1]").is_err()); // inverted
        assert!(parse_shape("String[..]").is_err()); // redundant with []
        assert!(parse_shape("String[a]").is_err()); // non-integer exact
        assert!(parse_shape("String[-1..]").is_err()); // negative bound
        assert!(parse_shape("String[3.5]").is_err()); // fractional
        assert!(parse_shape("String[1..2..3]").is_err()); // too many bounds
    }

    #[test]
    fn bare_bracket_count_is_not_a_cardinality() {
        // A cardinality is a SUFFIX and needs a base; a standalone `[3]` is a
        // would-be enum whose digit-first member fails the enum grammar.
        assert!(parse_shape("[3]").is_err());
        // With a base it is an exact count.
        assert_eq!(
            parse_shape("Number[3]"),
            Ok(list(Shape::Primitive(Primitive::Number), 3, Some(3)))
        );
    }

    #[test]
    fn range_cardinality_round_trips_through_display() {
        for src in [
            "String[]",
            "String[+]",
            "Number[3]",
            "String[3..]",
            "String[..5]",
            "String[2..5]",
        ] {
            let shape = parse_shape(src).unwrap();
            assert_eq!(shape.to_string(), src, "round-trip {src}");
        }
        // Explicit-range sugar renders as its canonical short form.
        assert_eq!(
            parse_shape("String[3..3]").unwrap().to_string(),
            "String[3]"
        );
        assert_eq!(parse_shape("String[0..]").unwrap().to_string(), "String[]");
        assert_eq!(parse_shape("String[1..]").unwrap().to_string(), "String[+]");
        assert_eq!(
            parse_shape("String[0..5]").unwrap().to_string(),
            "String[..5]"
        );
    }

    fn bound(value: &str, inclusive: bool) -> Bound {
        Bound {
            value: value.to_string(),
            inclusive,
        }
    }

    fn refined(base: Primitive, refinement: Refinement) -> Shape {
        Shape::Refined { base, refinement }
    }

    #[test]
    fn value_refinement_forms_parse() {
        assert_eq!(
            parse_shape("Number{>=0}"),
            Ok(refined(
                Primitive::Number,
                Refinement {
                    lower: Some(bound("0", true)),
                    ..Default::default()
                }
            ))
        );
        assert_eq!(
            parse_shape("Number{>=0 & integer}"),
            Ok(refined(
                Primitive::Number,
                Refinement {
                    lower: Some(bound("0", true)),
                    integer: true,
                    ..Default::default()
                }
            ))
        );
        assert_eq!(
            parse_shape("Number{>0 & <=1}"),
            Ok(refined(
                Primitive::Number,
                Refinement {
                    lower: Some(bound("0", false)),
                    upper: Some(bound("1", true)),
                    ..Default::default()
                }
            ))
        );
        assert_eq!(
            parse_shape("String{/^[a-z]+$/}"),
            Ok(refined(
                Primitive::String,
                Refinement {
                    pattern: Some("^[a-z]+$".to_string()),
                    ..Default::default()
                }
            ))
        );
        assert_eq!(
            parse_shape("Date{>=2020-01-01}"),
            Ok(refined(
                Primitive::Date,
                Refinement {
                    lower: Some(bound("2020-01-01", true)),
                    ..Default::default()
                }
            ))
        );
    }

    #[test]
    fn value_refinement_predicate_order_is_normalized() {
        // The meet is commutative, so token order does not change identity.
        assert_eq!(
            parse_shape("Number{integer & >=0}"),
            parse_shape("Number{>=0 & integer}")
        );
        assert_eq!(
            parse_shape("Number{<=1 & >=0}"),
            parse_shape("Number{>=0 & <=1}")
        );
    }

    #[test]
    fn value_refinement_number_literals_are_canonicalized() {
        assert_eq!(parse_shape("Number{>=0.0}"), parse_shape("Number{>=0}"));
        assert_eq!(parse_shape("Number{>=-0}"), parse_shape("Number{>=0}"));
        assert_eq!(parse_shape("Number{>=007}"), parse_shape("Number{>=7}"));
        assert_eq!(
            parse_shape("Number{<=1.50}").unwrap().to_string(),
            "Number{<=1.5}"
        );
        assert_eq!(
            parse_shape("Number{>=-1.5}").unwrap().to_string(),
            "Number{>=-1.5}"
        );
    }

    #[test]
    fn value_refinement_base_validity_is_enforced() {
        assert!(parse_shape("String{>=0}").is_err()); // comparison on String
        assert!(parse_shape("Number{/re/}").is_err()); // regex on Number
        assert!(parse_shape("Boolean{integer}").is_err()); // integer on Boolean
        assert!(parse_shape("Url{>=0}").is_err()); // comparison on Url
        assert!(parse_shape("String{integer}").is_err()); // integer on String
        assert!(parse_shape("rec{>=0}").is_err()); // refinement on a non-primitive
    }

    #[test]
    fn value_refinement_rejects_duplicate_and_empty() {
        assert!(parse_shape("Number{>=0 & >=1}").is_err()); // two lower bounds
        assert!(parse_shape("Number{<=0 & <=1}").is_err()); // two upper bounds
        assert!(parse_shape("Number{integer & integer}").is_err());
        assert!(parse_shape("String{/a/ & /b/}").is_err()); // two regexes
        assert!(parse_shape("Number{}").is_err()); // no predicate
        assert!(parse_shape("Number{>=0").is_err()); // missing closing brace
        assert!(parse_shape("Number{ & }").is_err()); // empty predicate
    }

    #[test]
    fn refinement_and_cardinality_errors_carry_dedicated_codes() {
        assert_eq!(
            parse_shape("String{>=0}").unwrap_err().code,
            REFINEMENT_BAD_SHAPE
        );
        assert_eq!(
            parse_shape("Number{>=0 & >=1}").unwrap_err().code,
            REFINEMENT_BAD_SHAPE
        );
        assert_eq!(
            parse_shape("Number{}").unwrap_err().code,
            REFINEMENT_BAD_SHAPE
        );
        assert_eq!(
            parse_shape("String[5..1]").unwrap_err().code,
            CARDINALITY_BAD_SHAPE
        );
        assert_eq!(
            parse_shape("String[-1..]").unwrap_err().code,
            CARDINALITY_BAD_SHAPE
        );
        // A reference suffix on a refined primitive is refinement-specific.
        assert_eq!(
            parse_shape("Number{>=0}*").unwrap_err().code,
            REFINEMENT_BAD_SHAPE
        );
        assert_eq!(
            parse_shape("Number{>=0}&").unwrap_err().code,
            REFINEMENT_BAD_SHAPE
        );
    }

    #[test]
    fn regex_predicate_is_delimiter_aware() {
        // A regex legitimately contains `{`, `}`, `&`, `|`, `[`, `]`, and an
        // escaped `/` — none of these are structural inside `/.../`.
        assert_eq!(
            parse_shape("String{/a{2,3}/}"),
            Ok(refined(
                Primitive::String,
                Refinement {
                    pattern: Some("a{2,3}".to_string()),
                    ..Default::default()
                }
            ))
        );
        assert_eq!(
            parse_shape("String{/a&b/}"),
            Ok(refined(
                Primitive::String,
                Refinement {
                    pattern: Some("a&b".to_string()),
                    ..Default::default()
                }
            ))
        );
        assert_eq!(
            parse_shape("String{/a\\/b/}"),
            Ok(refined(
                Primitive::String,
                Refinement {
                    pattern: Some("a\\/b".to_string()),
                    ..Default::default()
                }
            ))
        );
        assert_eq!(
            parse_shape("String{/]/}"),
            Ok(refined(
                Primitive::String,
                Refinement {
                    pattern: Some("]".to_string()),
                    ..Default::default()
                }
            ))
        );
    }

    #[test]
    fn refinement_composes_with_list_and_compound() {
        // A value refinement on the base, a range on the list.
        assert_eq!(
            parse_shape("Number{>=0}[3..]"),
            Ok(list(
                refined(
                    Primitive::Number,
                    Refinement {
                        lower: Some(bound("0", true)),
                        ..Default::default()
                    }
                ),
                3,
                None
            ))
        );
        // A refined scalar inside a union — the regex `|` must not split the union.
        assert_eq!(
            parse_shape("<String{/a|b/} | Number>"),
            Ok(Shape::Union(vec![
                refined(
                    Primitive::String,
                    Refinement {
                        pattern: Some("a|b".to_string()),
                        ..Default::default()
                    }
                ),
                Shape::Primitive(Primitive::Number),
            ]))
        );
    }

    #[test]
    fn value_refinement_round_trips_through_display() {
        for src in [
            "Number{>=0}",
            "Number{>=0 & integer}",
            "Number{>0 & <=1}",
            "String{/^[a-z]+$/}",
            "Date{>=2020-01-01}",
            "String{/a{2,3}/}",
            "Number{>=0}[3..]",
        ] {
            assert_eq!(
                parse_shape(src).unwrap().to_string(),
                src,
                "round-trip {src}"
            );
        }
    }

    #[test]
    fn non_empty_list_parses_with_plus_marker() {
        // [[type-def shape suffixes::au-type-system]]: `[+]` is an atomic list-suffix variant — peer of `[]`,
        // produces `Shape::List { min: 1, max: None }`.
        assert_eq!(
            parse_shape("String[+]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Primitive(Primitive::String)),
                min: 1,
                max: None,
            })
        );
        assert_eq!(
            parse_shape("rationale*[+]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Reference("rationale".into())),
                min: 1,
                max: None,
            })
        );
        assert_eq!(
            parse_shape("rationale&[+]"),
            Ok(Shape::List {
                inner: Box::new(Shape::InlineOrReference("rationale".into())),
                min: 1,
                max: None,
            })
        );
        assert_eq!(
            parse_shape("<A | B>[+]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Union(vec![
                    Shape::Record("A".into()),
                    Shape::Record("B".into()),
                ])),
                min: 1,
                max: None,
            })
        );
    }

    #[test]
    fn non_empty_list_round_trips_through_display() {
        for src in ["String[+]", "rationale*[+]", "<A | B>[+]", "[a, b][+]"] {
            let shape = parse_shape(src).unwrap_or_else(|e| panic!("parse {src}: {e:?}"));
            assert_eq!(format!("{}", shape), src, "Display round-trip for {src}");
        }
    }

    #[test]
    fn non_empty_marker_without_inner_is_syntax_error() {
        // `[+]` alone has no inner shape — load error.
        let err = parse_shape("[+]").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn old_trailing_plus_form_is_rejected() {
        // Pre-spec form `T[]+` is not a valid shape today — `+` is only
        // legal inside `[+]`. The whole string fails the type-name regex
        // at `parse_atom`, surfacing `shape-syntax-error`.
        let err = parse_shape("String[]+").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn list_of_each_primitive_parses() {
        for p in [
            Primitive::String,
            Primitive::Number,
            Primitive::Boolean,
            Primitive::Date,
            Primitive::DateTime,
            Primitive::Url,
        ] {
            let raw = format!("{}[]", p.as_str());
            assert_eq!(
                parse_shape(&raw),
                Ok(Shape::List {
                    inner: Box::new(Shape::Primitive(p)),
                    min: 0,
                    max: None
                }),
                "failed for {}",
                raw
            );
        }
    }

    #[test]
    fn list_of_enum_parses() {
        assert_eq!(
            parse_shape("[low, high][]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Enum(vec!["low".into(), "high".into(),])),
                min: 0,
                max: None
            })
        );
    }

    #[test]
    fn list_of_reference_parses() {
        assert_eq!(
            parse_shape("note*[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Reference("note".into())),
                min: 0,
                max: None
            })
        );
    }

    #[test]
    fn list_of_file_reference_parses() {
        assert_eq!(
            parse_shape("file*[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Reference("file".into())),
                min: 0,
                max: None
            })
        );
    }

    #[test]
    fn list_of_dotted_reference_parses() {
        assert_eq!(
            parse_shape("decision.decided*[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Reference("decision.decided".into())),
                min: 0,
                max: None
            })
        );
    }

    #[test]
    fn nested_list_parses() {
        // Spec doesn't enumerate `String[][]`, but it falls out of the
        // recursive grammar with no extra handling — accept it syntactically.
        assert_eq!(
            parse_shape("String[][]"),
            Ok(Shape::List {
                inner: Box::new(Shape::List {
                    inner: Box::new(Shape::Primitive(Primitive::String)),
                    min: 0,
                    max: None,
                }),
                min: 0,
                max: None,
            })
        );
    }

    #[test]
    fn list_with_invalid_inner_propagates_syntax_error() {
        // `String*[]` fails at the inner `*`-on-primitive check ([[type-def shape suffixes::au-type-system]]).
        let err = parse_shape("String*[]").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn list_of_bare_record_parses() {
        // `note[]` is a list of inline records.
        assert_eq!(
            parse_shape("note[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Record("note".into())),
                min: 0,
                max: None
            })
        );
    }

    #[test]
    fn list_of_inline_or_reference_parses() {
        assert_eq!(
            parse_shape("note&[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::InlineOrReference("note".into())),
                min: 0,
                max: None
            })
        );
    }

    #[test]
    fn empty_brackets_is_syntax_error() {
        // `[]` would mean either an empty enum or an empty list — both
        // illegal. The list path catches it first.
        let err = parse_shape("[]").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn whitespace_around_list_suffix_is_trimmed() {
        // `String []` (space before brackets) parses the same as `String[]`.
        assert_eq!(
            parse_shape("String []"),
            Ok(Shape::List {
                inner: Box::new(Shape::Primitive(Primitive::String)),
                min: 0,
                max: None
            })
        );
    }

    #[test]
    fn list_token_equality_is_structural() {
        let a = parse_shape("String[]").unwrap();
        let b = parse_shape("String[]").unwrap();
        let c = parse_shape("Number[]").unwrap();
        let d = parse_shape("String[][]").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c, "list of different inner shape is not equal");
        assert_ne!(a, d, "list-of-list is not equal to list");
        // Inner shape alone vs wrapped is distinct.
        assert_ne!(a, Shape::Primitive(Primitive::String));
    }

    #[test]
    fn empty_shape_is_malformed() {
        let err = parse_shape("").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn whitespace_only_is_malformed() {
        let err = parse_shape("   ").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn leading_digit_is_malformed() {
        let err = parse_shape("123abc").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn leading_punctuation_is_malformed() {
        let err = parse_shape("@foo").unwrap_err();
        assert_eq!(err.code, SHAPE_SYNTAX_ERROR);
    }

    #[test]
    fn primitive_as_str_round_trips() {
        for p in [
            Primitive::String,
            Primitive::Number,
            Primitive::Boolean,
            Primitive::Date,
            Primitive::DateTime,
            Primitive::Url,
        ] {
            assert_eq!(parse_shape(p.as_str()), Ok(Shape::Primitive(p)));
        }
    }

    #[test]
    fn shape_display_round_trips_through_parse_shape() {
        // Display renders the source-form slot expression. Parsing the
        // rendered string produces the same Shape — the AST is canonical.
        let cases: &[&str] = &[
            "String",
            "Date",
            "[low, moderate, high]",
            "decision*",
            "file*",
            "String[]",
            "decision*[]",
            "[a, b][]",
            "String[][]",
        ];
        for src in cases {
            let shape = parse_shape(src).unwrap();
            let rendered = shape.to_string();
            let reparsed = parse_shape(&rendered).unwrap();
            assert_eq!(shape, reparsed, "round-trip failed for {src:?}");
        }
    }

    // --- parse_shape_spanned: the type-def shape-layer spans ---

    /// Parse and return `(start, end, role)` per span, sorted by start. Asserts
    /// the shape itself parsed Ok.
    fn spans(raw: &str) -> Vec<(usize, usize, ShapeSpanRole)> {
        let (res, spans) = parse_shape_spanned(raw);
        assert!(res.is_ok(), "expected {raw:?} to parse: {res:?}");
        let mut out: Vec<_> = spans
            .iter()
            .map(|s| (s.range.start, s.range.end, s.role))
            .collect();
        out.sort_by_key(|t| t.0);
        out
    }

    #[test]
    fn spanned_parse_does_not_change_the_shape() {
        // parse_shape delegates to parse_shape_spanned, so the AST is identical.
        for src in [
            "String",
            "decision*",
            "<a | b>*",
            "type<mcp.tool>*",
            "[x, y][]",
        ] {
            assert_eq!(parse_shape(src), parse_shape_spanned(src).0, "{src}");
        }
    }

    #[test]
    fn reference_name_is_one_type_name_span() {
        assert_eq!(spans("decision*"), vec![(0, 8, ShapeSpanRole::TypeName)]);
    }

    #[test]
    fn record_and_inline_ref_names_are_type_name_spans() {
        assert_eq!(spans("rationale"), vec![(0, 9, ShapeSpanRole::TypeName)]);
        assert_eq!(spans("rationale&"), vec![(0, 9, ShapeSpanRole::TypeName)]);
    }

    #[test]
    fn primitives_any_and_file_are_builtin_spans() {
        assert_eq!(spans("Number"), vec![(0, 6, ShapeSpanRole::Builtin)]);
        assert_eq!(spans("any"), vec![(0, 3, ShapeSpanRole::Builtin)]);
        // file* parses as a Reference("file"), but `file` is a built-in keyword.
        assert_eq!(spans("file*"), vec![(0, 4, ShapeSpanRole::Builtin)]);
    }

    #[test]
    fn span_offset_accounts_for_leading_whitespace() {
        // Spans are relative to the raw (untrimmed) string.
        assert_eq!(spans("  String  "), vec![(2, 8, ShapeSpanRole::Builtin)]);
    }

    #[test]
    fn list_suffix_adds_no_span() {
        // The list wrapper is punctuation; only the inner name is a span.
        assert_eq!(spans("decision*[]"), vec![(0, 8, ShapeSpanRole::TypeName)]);
        assert_eq!(spans("decision*[+]"), vec![(0, 8, ShapeSpanRole::TypeName)]);
    }

    #[test]
    fn def_ref_records_the_type_keyword_and_the_bound() {
        // type<mcp.tool>* : `type` builtin at 0..4, `mcp.tool` type-name at 5..13.
        assert_eq!(
            spans("type<mcp.tool>*"),
            vec![
                (0, 4, ShapeSpanRole::Builtin),
                (5, 13, ShapeSpanRole::TypeName),
            ]
        );
        // unconstrained type* : just the keyword.
        assert_eq!(spans("type*"), vec![(0, 4, ShapeSpanRole::Builtin)]);
    }

    #[test]
    fn def_ref_compound_bound_records_each_ceiling() {
        // type<a | b>* : `type` builtin, then `a` and `b` type-names.
        assert_eq!(
            spans("type<a | b>*"),
            vec![
                (0, 4, ShapeSpanRole::Builtin),
                (5, 6, ShapeSpanRole::TypeName),
                (9, 10, ShapeSpanRole::TypeName),
            ]
        );
    }

    #[test]
    fn compound_reference_records_each_operand() {
        // <a | b>* : operands a (1..2) and b (5..6), both type-names.
        assert_eq!(
            spans("<a | b>*"),
            vec![
                (1, 2, ShapeSpanRole::TypeName),
                (5, 6, ShapeSpanRole::TypeName),
            ]
        );
    }

    #[test]
    fn plain_union_records_builtins_and_names() {
        // <String | decision> : a builtin and a type-name.
        assert_eq!(
            spans("<String | decision>"),
            vec![
                (1, 7, ShapeSpanRole::Builtin),
                (10, 18, ShapeSpanRole::TypeName),
            ]
        );
    }

    #[test]
    fn enum_literals_are_enum_member_spans() {
        // [low, high] : low at 1..4, high at 6..10.
        assert_eq!(
            spans("[low, high]"),
            vec![
                (1, 4, ShapeSpanRole::EnumMember),
                (6, 10, ShapeSpanRole::EnumMember),
            ]
        );
    }

    #[test]
    fn pinned_reference_records_the_inner_name() {
        // T*@ : the `@`/`*` are punctuation; only the name is a span.
        assert_eq!(spans("decision*@"), vec![(0, 8, ShapeSpanRole::TypeName)]);
    }

    #[test]
    fn parse_error_yields_no_spans() {
        // A name was recorded mid-descent, then the compound suffix rejected a
        // primitive operand — the partial spans are discarded.
        let (res, spans) = parse_shape_spanned("<String | decision>*");
        assert!(res.is_err());
        assert!(
            spans.is_empty(),
            "error path must discard partial spans: {spans:?}"
        );
    }

    #[test]
    fn every_span_is_a_valid_slice_of_the_input() {
        // Guards the pointer-arithmetic offset recovery across forms.
        for raw in [
            "decision*",
            "  Number  ",
            "type<mcp.tool>*",
            "<a | b>*[]",
            "[low, moderate, high]",
            "rationale&",
        ] {
            let (_res, spans) = parse_shape_spanned(raw);
            for s in &spans {
                assert!(
                    s.range.end <= raw.len() && s.range.start < s.range.end,
                    "span {:?} out of bounds for {raw:?}",
                    s.range
                );
            }
        }
    }

    // ----- `::repo` peer qualifier on a type name -----

    fn qn(base: &str, repo: &str) -> QualifiedName {
        QualifiedName {
            base: base.to_string(),
            repo: Some(repo.to_string()),
        }
    }

    #[test]
    fn unqualified_name_has_no_repo() {
        // The bare path stays own(): repo is None, byte-identical behaviour to
        // the pre-qualifier String.
        assert_eq!(
            parse_shape("rationale"),
            Ok(Shape::Record(QualifiedName::own("rationale")))
        );
        assert_eq!(
            parse_shape("rationale*"),
            Ok(Shape::Reference(QualifiedName::own("rationale")))
        );
        assert_eq!(QualifiedName::own("rationale").repo, None);
    }

    #[test]
    fn qualified_name_parses_at_every_single_name_position() {
        // Record, reference, inline-or-reference — the three single-name forms.
        assert_eq!(
            parse_shape("foo::repo"),
            Ok(Shape::Record(qn("foo", "repo")))
        );
        assert_eq!(
            parse_shape("foo::repo*"),
            Ok(Shape::Reference(qn("foo", "repo")))
        );
        assert_eq!(
            parse_shape("foo::repo&"),
            Ok(Shape::InlineOrReference(qn("foo", "repo")))
        );
        // Dotted sealed-leaf base stays valid under a qualifier.
        assert_eq!(
            parse_shape("decision.decided::base*"),
            Ok(Shape::Reference(qn("decision.decided", "base")))
        );
    }

    #[test]
    fn qualified_name_under_list_and_pin_suffixes() {
        // The qualifier binds to the name, before `*` / `@` / `[]`.
        assert_eq!(
            parse_shape("foo::repo*[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Reference(qn("foo", "repo"))),
                min: 0,
                max: None,
            })
        );
        assert_eq!(
            parse_shape("foo::repo*@[]"),
            Ok(Shape::List {
                inner: Box::new(Shape::Pinned(Box::new(Shape::Reference(qn("foo", "repo"))))),
                min: 0,
                max: None,
            })
        );
    }

    #[test]
    fn qualified_names_in_a_compound() {
        // Per-element qualification, mixed with an own-repo branch.
        assert_eq!(
            parse_shape("<a::r1 | b::r2 | c>*"),
            Ok(Shape::CompoundReference {
                mode: RefMode::Star,
                op: CompoundRefOp::Union,
                branches: vec![qn("a", "r1"), qn("b", "r2"), QualifiedName::own("c")],
            })
        );
    }

    #[test]
    fn qualified_names_in_a_def_bound() {
        assert_eq!(
            parse_shape("type<t::r>*"),
            Ok(Shape::DefReference(Some(DefBound::Single(qn("t", "r")))))
        );
        assert_eq!(
            parse_shape("type<a::r1 | b::r2>*"),
            Ok(Shape::DefReference(Some(DefBound::Compound {
                op: CompoundRefOp::Union,
                branches: vec![qn("a", "r1"), qn("b", "r2")],
            })))
        );
    }

    #[test]
    fn qualified_forms_roundtrip_through_display() {
        // Forms whose Display is byte-identical to the source. The compound
        // reference is excluded: its Display canonicalizes to the
        // `*`-per-branch form (see `qualified_compound_display_carries_the_repo`).
        for src in [
            "foo::repo",
            "foo::repo*",
            "foo::repo&",
            "foo::repo*[]",
            "foo::repo*@[]",
            "type<t::r>*",
            "type<a::r1 | b::r2>*",
        ] {
            let shape = parse_shape(src).unwrap();
            assert_eq!(shape.to_string(), src, "Display mismatch for {src}");
        }
    }

    #[test]
    fn qualified_compound_display_carries_the_repo() {
        // The compound reference renders the `*`-per-branch canonical form, and
        // the `::repo` qualifier rides on each branch name.
        let shape = parse_shape("<a::r1 | b::r2 | c>*").unwrap();
        assert_eq!(shape.to_string(), "<a::r1* | b::r2* | c*>*");
        // It re-parses to the same AST, the round-trip property that matters.
        assert_eq!(parse_shape(&shape.to_string()), Ok(shape));
    }

    #[test]
    fn empty_repo_scope_is_an_error() {
        // `foo::` is the shape sibling of `wikilink-empty-repo`. There is no
        // `::@commit` form in shape position (the `@` postfix carries no sha).
        for bad in ["foo::", "foo::*", "::repo", "::"] {
            let err = parse_shape(bad).unwrap_err();
            assert_eq!(err.code, SHAPE_SYNTAX_ERROR, "'{bad}' should be rejected");
        }
    }

    #[test]
    fn malformed_repo_name_is_an_error() {
        // A second `::` lands in the repo half; a space / bad char fails the
        // repo-name regex.
        for bad in ["foo::a::b", "foo::bad name", "foo::1repo", "foo::a.b"] {
            let err = parse_shape(bad).unwrap_err();
            assert_eq!(err.code, SHAPE_SYNTAX_ERROR, "'{bad}' should be rejected");
        }
    }

    #[test]
    fn repo_qualifier_on_a_builtin_is_an_error() {
        // `::repo` names a peer type-def, never a built-in sentinel.
        for bad in [
            "String::repo",
            "String::repo*",
            "file::repo*",
            "any::repo*",
            "any::repo",
            "type::repo*",
        ] {
            let err = parse_shape(bad).unwrap_err();
            assert_eq!(err.code, SHAPE_SYNTAX_ERROR, "'{bad}' should be rejected");
        }
    }

    #[test]
    fn qualified_name_records_the_whole_token_span() {
        // The span covers `foo::repo`, classified as a navigable TypeName.
        assert_eq!(spans("foo::repo*"), vec![(0, 9, ShapeSpanRole::TypeName)]);
    }

    /// A well-formed `Name(...)` yields the name and its trimmed positional args.
    #[test]
    fn tuple_parses_and_round_trips() {
        // A fixed-arity product; elements order-significant, each a full shape.
        let cases = [
            "(Number, Number)",
            "(Number, Number, Number, Number)",
            "(String, Number)",
            "(point, point)",
            "(<a | b>, Number)",
            "(Number{>=0 & <=100}, String)",
        ];
        for src in cases {
            let shape = parse_shape(src).unwrap_or_else(|e| panic!("parse {src:?}: {e:?}"));
            assert!(matches!(shape, Shape::Tuple(_)), "{src:?} => {shape:?}");
            assert_eq!(shape.to_string(), src, "{src:?} does not round-trip");
        }
    }

    #[test]
    fn tuple_element_order_is_significant() {
        assert_ne!(
            parse_shape("(String, Number)").unwrap(),
            parse_shape("(Number, String)").unwrap(),
        );
    }

    #[test]
    fn a_nested_tuple_parses() {
        let shape = parse_shape("(point, (Number, Number))").unwrap();
        let Shape::Tuple(elements) = shape else {
            panic!("expected a tuple");
        };
        assert_eq!(
            elements.len(),
            2,
            "top-level comma inside the nested tuple does not split"
        );
        assert!(matches!(elements[1], Shape::Tuple(_)));
    }

    #[test]
    fn an_empty_tuple_is_rejected() {
        assert!(parse_shape("()").is_err());
    }

    #[test]
    fn a_list_of_tuples_parses() {
        // `(A, B)[]` strips the list suffix, leaving the tuple inside.
        let shape = parse_shape("(Number, Number)[]").unwrap();
        let Shape::List { inner, .. } = shape else {
            panic!("expected a list");
        };
        assert!(matches!(*inner, Shape::Tuple(_)));
    }

    #[test]
    fn a_tuple_inside_a_union_parses() {
        // Exercises the paren-aware compound helpers (`has_top_level_op`,
        // `split_top_level`): the tuple's inner comma must not split the union,
        // and a `|`/`&` inside the tuple's parens must not be read as the
        // compound operator (code-review 3.2).
        let shape = parse_shape("<(Number, Number) | paper>").unwrap();
        let Shape::Union(branches) = shape else {
            panic!("expected a union");
        };
        assert_eq!(
            branches.len(),
            2,
            "the tuple's comma does not split the union"
        );
        assert!(matches!(branches[0], Shape::Tuple(_)));
        assert!(matches!(branches[1], Shape::Record(_)));
    }

    #[test]
    fn recognize_constructor_extracts_name_and_args() {
        let ctor = |raw: &str| match recognize_constructor(raw) {
            ConstructorMatch::Constructor(c) => c,
            other => panic!("{raw:?} should be a constructor, got {other:?}"),
        };
        // a scalar brand, one arg
        assert_eq!(
            ctor("meter(42)"),
            Constructor {
                name: "meter".into(),
                args: vec!["42".into()]
            }
        );
        // a named-enum member
        assert_eq!(
            ctor("quality(reviewed)"),
            Constructor {
                name: "quality".into(),
                args: vec!["reviewed".into()]
            }
        );
        // a tuple, args split on top-level commas and trimmed
        assert_eq!(
            ctor("point(20, 30)"),
            Constructor {
                name: "point".into(),
                args: vec!["20".into(), "30".into()]
            }
        );
        // surrounding whitespace on the whole value is ignored
        assert_eq!(
            ctor("  meter(42)  "),
            Constructor {
                name: "meter".into(),
                args: vec!["42".into()]
            }
        );
        // a `::repo`-qualified brand name
        assert_eq!(
            ctor("icon-role::sdk(save)"),
            Constructor {
                name: "icon-role::sdk".into(),
                args: vec!["save".into()]
            }
        );
        // a dotted sealed-leaf-style name
        assert_eq!(
            ctor("fam.leaf(x)"),
            Constructor {
                name: "fam.leaf".into(),
                args: vec!["x".into()]
            }
        );
        // zero args
        assert_eq!(
            ctor("meter()"),
            Constructor {
                name: "meter".into(),
                args: vec![]
            }
        );
        assert_eq!(
            ctor("meter(  )"),
            Constructor {
                name: "meter".into(),
                args: vec![]
            }
        );
    }

    /// Splitting is paren-depth and quote aware: a comma inside a nested
    /// constructor or a quoted string never splits an argument.
    #[test]
    fn recognize_tuple_handles_the_nameless_paren_form() {
        // a nameless paren tuple splits at top level
        assert_eq!(
            recognize_tuple("(20, 30)"),
            TupleMatch::Tuple(vec!["20".into(), "30".into()])
        );
        // nested tuples: inner commas do not split
        assert_eq!(
            recognize_tuple("((1,2), (3,4))"),
            TupleMatch::Tuple(vec!["(1,2)".into(), "(3,4)".into()])
        );
        // a nameless tuple of brand constructors
        assert_eq!(
            recognize_tuple("(color(1), color(2))"),
            TupleMatch::Tuple(vec!["color(1)".into(), "color(2)".into()])
        );
        // a bracket list is never a tuple
        assert_eq!(recognize_tuple("[20, 30]"), TupleMatch::NotTuple);
        // a named constructor is not a nameless tuple
        assert_eq!(recognize_tuple("point(20, 30)"), TupleMatch::NotTuple);
        // unbalanced / trailing content is malformed
        assert_eq!(recognize_tuple("(20, 30"), TupleMatch::Malformed);
        assert_eq!(recognize_tuple("(20, 30)x"), TupleMatch::Malformed);
    }

    #[test]
    fn recognize_constructor_splits_only_at_top_level() {
        let ctor = |raw: &str| match recognize_constructor(raw) {
            ConstructorMatch::Constructor(c) => c,
            other => panic!("{raw:?} should be a constructor, got {other:?}"),
        };
        // a tuple of nested constructors: the inner commas do not split
        assert_eq!(
            ctor("rect(point(1,2), point(3,4))"),
            Constructor {
                name: "rect".into(),
                args: vec!["point(1,2)".into(), "point(3,4)".into()],
            }
        );
        // a comma inside a quoted string is literal
        assert_eq!(
            ctor(r#"label("a, b")"#),
            Constructor {
                name: "label".into(),
                args: vec![r#""a, b""#.into()]
            }
        );
        // a paren inside a quoted string does not open depth
        assert_eq!(
            ctor(r#"label("x(y")"#),
            Constructor {
                name: "label".into(),
                args: vec![r#""x(y""#.into()]
            }
        );
        // a comma inside a square-bracket (list) argument does not split
        assert_eq!(
            ctor("foo([1, 2, 3], x)"),
            Constructor {
                name: "foo".into(),
                args: vec!["[1, 2, 3]".into(), "x".into()]
            }
        );
        // a comma inside an angle-bracket (union) argument does not split
        assert_eq!(
            ctor("foo(<a | b>, x)"),
            Constructor {
                name: "foo".into(),
                args: vec!["<a | b>".into(), "x".into()]
            }
        );
    }

    /// `'` is NOT a quote delimiter, so an apostrophe in a value is an ordinary
    /// character and does not open an unterminated quote.
    #[test]
    fn recognize_constructor_allows_an_apostrophe() {
        let ctor = |raw: &str| match recognize_constructor(raw) {
            ConstructorMatch::Constructor(c) => c,
            other => panic!("{raw:?} should be a constructor, got {other:?}"),
        };
        assert_eq!(
            ctor("name(O'Brien)"),
            Constructor {
                name: "name".into(),
                args: vec!["O'Brien".into()]
            }
        );
        assert_eq!(
            ctor("label(it's fine)"),
            Constructor {
                name: "label".into(),
                args: vec!["it's fine".into()]
            }
        );
    }

    /// A value with no `Name(` prefix is not a constructor: a bare scalar, a bare
    /// member, prose with a spaced paren.
    #[test]
    fn recognize_constructor_passes_through_non_constructors() {
        for raw in [
            "42",
            "reviewed",
            "https://x.test",
            "save (the file)",
            "(20, 30)",
            "",
        ] {
            assert_eq!(
                recognize_constructor(raw),
                ConstructorMatch::NotConstructor,
                "{raw:?} should not be a constructor"
            );
        }
    }

    /// A value that starts a `Name(` shape but does not close cleanly is malformed:
    /// unbalanced parens, or trailing content after the close.
    #[test]
    fn recognize_constructor_flags_malformed() {
        for raw in [
            "meter(42",     // unterminated
            "meter(42)x",   // trailing content
            "meter(1,(2)",  // unbalanced nested
            "meter())",     // extra close
            "meter(a) (b)", // two groups
        ] {
            assert_eq!(
                recognize_constructor(raw),
                ConstructorMatch::Malformed,
                "{raw:?} should be malformed"
            );
        }
    }
}
