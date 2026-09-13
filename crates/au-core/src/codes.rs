//! Diagnostic codes emitted by `au-core`. Stable kebab-case strings; downstream
//! consumers match on these.

use au_diagnostics::DiagnosticCode;

// ----- type-def parser-level shape issues -----

/// `extends:` is neither a string nor a list of strings.
pub const PARENT_CLAIM_BAD_SHAPE: DiagnosticCode =
    DiagnosticCode::from_static("parent-claim-bad-shape");

/// A top-level `type:` key on a type-def. On a type-def the inheritance claim
/// is `extends:`; `type:` is the identity claim, which a type-def never makes of
/// itself. Fires a targeted error rather than dropping the key as
/// `unknown-top-level-key`, so a mistaken parent claim never silently strips
/// the parent. See [[type-def extends::au-type-system]].
pub const TYPE_KEY_ON_TYPE_DEF: DiagnosticCode =
    DiagnosticCode::from_static("type-key-on-type-def");

/// `fields:` is not a map (the retired list form, or any non-map value).
pub const FIELDS_NOT_A_MAP: DiagnosticCode = DiagnosticCode::from_static("fields-not-a-map");

/// A `fields:` entry has a bad key: a non-scalar key, or a name that violates
/// the field-name grammar.
pub const FIELD_DECL_BAD_SHAPE: DiagnosticCode =
    DiagnosticCode::from_static("field-decl-bad-shape");

/// The same field name is declared twice in one type-def's `fields:` map. A
/// duplicate mapping key; saphyr keeps only the last, so the earlier decl is
/// silently dropped without this signal.
pub const DUPLICATE_FIELD: DiagnosticCode = DiagnosticCode::from_static("duplicate-field");

/// `sealed:` is not a list of strings.
pub const SEALED_BAD_SHAPE: DiagnosticCode = DiagnosticCode::from_static("sealed-bad-shape");

/// The `abstract:` marker's value is not a boolean. `abstract: true` marks a
/// type-def non-claimable, `false` or absent is concrete. See
/// [[spec - abstract type-defs - a non-claimable open type-def, sealed is abstract plus closed]].
pub const ABSTRACT_MARKER_BAD_SHAPE: DiagnosticCode =
    DiagnosticCode::from_static("abstract-marker-bad-shape");

/// A `shape:` brand value is not one of the four brand forms: an empty enum, a
/// non-string or non-legal-name enum member (a member with an embedded comma
/// would canonicalize ambiguously), a value that is neither a shape-expression
/// scalar nor an enum member list, or a parseable shape that is not a brand form (a bare
/// record name, a `*` / `&` reference, a `<...>*` compound reference, a `type*`
/// def-ref, `any`, a list, a pin, an intersection). A brand names a scalar,
/// enum, union, or tuple. A bad shape EXPRESSION (a malformed `<A | B>` string) is
/// `shape-syntax-error` instead, from the slot grammar. See
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
pub const MALFORMED_BRAND_SHAPE: DiagnosticCode =
    DiagnosticCode::from_static("malformed-brand-shape");

/// A type-def declares both `shape:` (a brand) and a record key (`fields:` /
/// `sealed:` / `body:`). A def is a brand XOR a record: a brand names an
/// underlying shape and has no fields. See
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
pub const BRAND_WITH_RECORD_KEYS: DiagnosticCode =
    DiagnosticCode::from_static("brand-with-record-keys");

/// A field slot demands a brand with a `*` or `&` reference suffix the brand
/// does not admit. A NOMINAL brand (scalar / enum / tuple) is inline-only, the
/// same as the primitive it wraps, so it is used by bare name. A STRUCTURAL
/// (union) brand is referenceable ONLY when every member is a record type; a
/// union with a primitive or nominal-brand member (`<evidence | String>`,
/// `<meter | second>`) is inline-only too. See
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
pub const BRAND_NOT_REFERENCEABLE: DiagnosticCode =
    DiagnosticCode::from_static("brand-not-referenceable");

/// An explicit `Name(...)` constructor names a type the slot does not admit. The
/// admitted names are the slot's own brand, or a member of its union (records,
/// nominal brands, primitives alike): a `second(42)` foreign brand in a `meter`
/// slot, or a `Number(42)` reserved primitive in a `meter` slot (admitted only as
/// a union member). A bare value coerces silently; a reserved-primitive
/// constructor IS admitted as a union member, the literal escape
/// (`String("looks(foo)")` at `<String | label>`). See
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
pub const BRAND_CONSTRUCTOR_MISMATCH: DiagnosticCode =
    DiagnosticCode::from_static("brand-constructor-mismatch");

/// A value starts a `Name(` constructor shape but does not close as a
/// well-formed one: an unbalanced paren, or content after the closing `)`. A
/// `Warning`, the value still reads as a plain scalar against the slot. See
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
pub const MALFORMED_CONSTRUCTOR: DiagnosticCode =
    DiagnosticCode::from_static("malformed-constructor");

/// A tuple value's element count differs from the declared arity: a
/// `point(20, 30, 40)` at a `(Number, Number)` slot, or a bare sequence of the
/// wrong length. A tuple is a fixed-arity product, every element required. See
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
pub const TUPLE_ARITY_MISMATCH: DiagnosticCode =
    DiagnosticCode::from_static("tuple-arity-mismatch");

/// A bare value at a union brand slot cannot be discriminated to one member: a
/// value two or more admitting members accept, sharing one base primitive with no
/// distinguishing predicate (`"hi"` at `<String | label>`, `42` at
/// `<meter | second>`), or an inline record with no `type:` to pick a record
/// member. Name the branch with a `Name(...)` constructor. A single unambiguous
/// branch coerces; a distinguishing predicate (a `Url` prefix, a `Date` format, a
/// refinement, an enum membership) discriminates, so `<String | Url>` /
/// `<String | meter>` never fire this. See
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
pub const BRAND_CONSTRUCTOR_REQUIRED: DiagnosticCode =
    DiagnosticCode::from_static("brand-constructor-required");

/// A `meta:` sub-region names a type whose resolved closure does not include the
/// engine meta marker (`au.engine.meta`). Meta-ness is nominal: a meta type must
/// mix in the marker. See
/// [[spec - meta type marker - the meta position admits only types that mix in the engine meta base]].
pub const NON_META_TYPE_IN_META_POSITION: DiagnosticCode =
    DiagnosticCode::from_static("non-meta-type-in-meta-position");

/// A `required:` meta item's value is not a meta type name or a list of names.
/// See [[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]].
pub const REQUIRED_META_BAD_SHAPE: DiagnosticCode =
    DiagnosticCode::from_static("required-meta-bad-shape");

/// A `required:` names a type absent from the graph. The meta sibling of
/// `slot-references-absent-type`. A `::repo` target reuses the `type-repo-*`
/// gate instead. See
/// [[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]].
pub const REQUIRED_META_ABSENT_TYPE: DiagnosticCode =
    DiagnosticCode::from_static("required-meta-absent-type");

/// A `required:` names a present type that is not a meta type, its closure lacks
/// the engine meta marker `au.engine.meta`. The base-side sibling of
/// `non-meta-type-in-meta-position`. See
/// [[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]].
pub const REQUIRED_META_NOT_A_META_TYPE: DiagnosticCode =
    DiagnosticCode::from_static("required-meta-not-a-meta-type");

/// A concrete (non-abstract) type whose closure includes a base declaring a
/// `required:` obligation does not itself declare a satisfying meta block. A
/// `warning`, advisory per the open-world stance. Satisfaction is literal (an
/// ancestor's surfaced block does not count) and by closure (a subtype of the
/// required meta satisfies). See
/// [[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]].
pub const SUBTYPE_MISSING_REQUIRED_META: DiagnosticCode =
    DiagnosticCode::from_static("subtype-missing-required-meta");

/// A claim (file-level, inline record, or meta sub-region) names a declared-
/// abstract, non-sealed type. Abstract types are non-claimable; claim a concrete
/// subtype. A sealed claim keeps firing the more specific `sealed-parent-claimed`,
/// so the two never double-fire. See
/// [[spec - abstract type-defs - a non-claimable open type-def, sealed is abstract plus closed]].
pub const ABSTRACT_TYPE_CLAIMED: DiagnosticCode =
    DiagnosticCode::from_static("abstract-type-claimed");

/// `meta:` is not a list of mappings.
pub const META_NOT_A_LIST: DiagnosticCode = DiagnosticCode::from_static("meta-not-a-list");

/// A meta block is missing a `type:` key, or the key's value isn't a string.
pub const META_BLOCK_BAD_SHAPE: DiagnosticCode =
    DiagnosticCode::from_static("meta-block-bad-shape");

/// A meta sub-region's `type:` is a list (mixin form). Spec [[type-def meta::au-type-system]] forbids
/// mixin inside `meta:` sub-regions — the discriminator must be a bare scalar
/// per [[type list form::au-type-system]]. Distinct from `META_BLOCK_BAD_SHAPE` (which covers absent / non-
/// string `type:`) so consumers can match on the specific authoring error.
pub const META_MIXIN_NOT_SUPPORTED: DiagnosticCode =
    DiagnosticCode::from_static("meta-mixin-not-supported");

/// Top-level type-def value isn't a YAML mapping.
pub const TYPE_DEF_NOT_A_MAPPING: DiagnosticCode =
    DiagnosticCode::from_static("type-def-not-a-mapping");

// ----- graph build -----

/// Two or more files derive the same type name.
pub const DUPLICATE_TYPE_DEF: DiagnosticCode = DiagnosticCode::from_static("duplicate-type-def");

// ----- graph-structure load checks -----

/// Type name does not match `^[A-Za-z][A-Za-z0-9_-]*(\.[A-Za-z][A-Za-z0-9_-]*)*$`.
pub const TYPE_NAME_VIOLATES_REGEX: DiagnosticCode =
    DiagnosticCode::from_static("type-name-violates-regex");

/// `type:` chain has a cycle. Also fires for a cross-repo cycle (`H` extends
/// `B::repo`, `B` extends `H::repo`), surfaced from the resolution graph's
/// resolved parent edges since the own-graph check sees one graph.
pub const CYCLE_IN_TYPE_CHAIN: DiagnosticCode = DiagnosticCode::from_static("cycle-in-type-chain");

/// `type:` ancestor walk hit `MAX_TYPE_CHAIN_DEPTH` without terminating.
/// Either a pathological deep chain or — if the cycle check missed it — a
/// cycle slipping past `cycle-in-type-chain`. Defensive guard so the
/// recursive DFS can't blow the stack on adversarial knowledge base content.
pub const TYPE_CHAIN_DEPTH_EXCEEDED: DiagnosticCode =
    DiagnosticCode::from_static("type-chain-depth-exceeded");

/// Two `meta:` sub-regions on the same host type-def use the same `type:` discriminator.
pub const DUPLICATE_META_BLOCK: DiagnosticCode =
    DiagnosticCode::from_static("duplicate-meta-block");

// ----- inheritance load checks -----

/// A subtype declares a field name an ancestor already carries. Width-only
/// subtyping forbids redeclaration even with identical shape.
pub const FIELD_REDECLARATION: DiagnosticCode = DiagnosticCode::from_static("field-redeclaration");

/// A type-def's `type:` chain crosses a sealed parent without going through any
/// of the parent's listed branches.
pub const SEALED_NO_SURPRISE_CHILDREN: DiagnosticCode =
    DiagnosticCode::from_static("sealed-no-surprise-children");

/// A sealed type-def also declares `abstract: true`. Sealed already carries
/// non-claimability (it is abstract plus a closed branch set), so the explicit
/// marker is redundant. Advisory hint, see
/// [[spec - abstract type-defs - a non-claimable open type-def, sealed is abstract plus closed]].
///
/// There is deliberately NO "abstract type with no concrete descendant" code.
/// An abstract interface defined for other repos (or future work) to extend is a
/// legitimate, stable state, not a defect. Cross-repo is the usual topology.
pub const REDUNDANT_ABSTRACT_ON_SEALED: DiagnosticCode =
    DiagnosticCode::from_static("redundant-abstract-on-sealed");

/// A user type-def claims a name reserved by the engine: `file` (built-in
/// any-repo-file reference target, spec [[type-def shape file::au-type-system]]), `any` (the no-type slot shape,
/// spec [[type-def shape any::au-type-system]]), `opaque` (the uninterpreted slot shape, spec [[type-def shape opaque::au-type-system]]),
/// or one of the primitive shape names `String`, `Number`,
/// `Boolean`, `Date`, `DateTime`, `Url` (spec [[type-def legal names::au-type-system]], [[type-def shape primitive::au-type-system]]).
pub const RESERVED_TYPE_NAME: DiagnosticCode = DiagnosticCode::from_static("reserved-type-name");

/// A `Shape::Reference` slot names a type-def absent from the type graph
/// (and isn't the built-in `file`). Walks `Shape::List` wrappers transparently.
pub const SLOT_REFERENCES_ABSENT_TYPE: DiagnosticCode =
    DiagnosticCode::from_static("slot-references-absent-type");

/// A `type:` parent names a type-def absent from the type graph, so the closure
/// truncates and the parent's fields silently vanish. The parent sibling of
/// `slot-references-absent-type`; fires for every type-def.
pub const PARENT_REFERENCES_ABSENT_TYPE: DiagnosticCode =
    DiagnosticCode::from_static("parent-references-absent-type");

/// Resolved reference target's `type:` closure does not include the slot's
/// required type. Producer is the instance validator; co-locating with the
/// other reference codes here keeps all "validator emits this" codes on the
/// same side of the crate boundary. (au-references owns resolution-side codes
/// — target-missing, target-ambiguous, case-collision-basename — and never
/// fires this one itself.)
///
/// A `[[name::repo]]` target is checked across the repo boundary by comparing
/// the two repos' versions of the required type across their whole closure,
/// member by member as `(name, canonical-hash)`. A name match, or even a
/// top-level hash match, is not enough: two repos can write the same
/// `myType (type: foo)` while their `foo` diverges, so `myType` hashes equal yet
/// the effective contracts differ.
///
/// A qualified DEMANDED type (`foo::repo*`) checks the peer type by `TypeId`
/// membership over the target's FOLDED closure: the demanded
/// `(foo, repo.closure_id(foo))` must be reached by the target's own claims,
/// resolved over its repo's resolution graph. A `^block-id` target folds its own
/// block claim, an inline record's or a body typed-fence's.
pub const REFERENCE_TARGET_TYPE_MISMATCH: DiagnosticCode =
    DiagnosticCode::from_static("reference-target-type-mismatch");

/// A `type<...>*` / `type*` def-reference resolved to a target that exists but
/// is not a type-def ([[type-def shape def-ref::au-type-system]]). The def axis demands a `.type.yaml`
/// target; a plain instance or asset does not qualify. Distinct from the
/// existence side (`reference-target-missing`) — the target is present, just
/// not a def. Producer is the instance validator.
pub const DEF_REF_TARGET_NOT_A_TYPE_DEF: DiagnosticCode =
    DiagnosticCode::from_static("def-ref-target-not-a-type-def");

/// A constrained `type<T>*` def-reference resolved to a type-def whose `type:`
/// parent closure does not include `T` ([[type-def shape def-ref::au-type-system]]). The def-axis
/// sibling of `reference-target-type-mismatch`: that code checks the target
/// instance's identity closure, this one checks the target def's parent
/// closure. A compound bound (`type<a | b>*`) is any-of for a union, all-of for
/// an intersection. A `::repo` ceiling (`type<baz::repo>*`) checks the ceiling's
/// peer identity against the target def's FOLDED parent closure, the def-axis
/// qualified demand. The unconstrained `type*` never fires this. Producer is the
/// instance validator.
pub const DEF_REF_CLOSURE_MISMATCH: DiagnosticCode =
    DiagnosticCode::from_static("def-ref-closure-mismatch");

/// A value in a `*@` enforced-pinned slot ([[type-def shape suffixes::au-type-system]]) is not a
/// commit-pinned reference. The slot's contract is that every value carries a
/// `@commit` pin; an unpinned wikilink, or a non-reference value, violates it.
/// Distinct from the resolution outcomes — this is the shape-level requirement,
/// checked before resolution. See
/// [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
pub const VALUE_NOT_PINNED: DiagnosticCode = DiagnosticCode::from_static("value-not-pinned");

/// A commit-pinned value (`[[file::@sha]]`) in a slot whose shape admits no pin.
/// The mirror of `value-not-pinned`: a `*@` slot demands a pin, a plain `T*` /
/// `T&` slot forbids one. A pin is legal only where the shape admits it, a `*@`
/// shape or a `*@` branch of a union; fires only when NO branch of the slot
/// admits a pin. A bare commit-referent (`[[::@sha]]`, empty target) is exempt,
/// it names a commit not a file. See
/// [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
pub const UNEXPECTED_COMMIT_PIN: DiagnosticCode =
    DiagnosticCode::from_static("unexpected-commit-pin");

// ----- instance parser -----

/// Top-level frontmatter on an instance is not a YAML mapping.
pub const INSTANCE_NOT_A_MAPPING: DiagnosticCode =
    DiagnosticCode::from_static("instance-not-a-mapping");

/// Instance has no top-level `type:` key.
///
/// Direct callers of `parse_instance` get this diagnostic. The engine's build
/// does NOT — it classifies first (`frontmatter_has_type`) and routes a
/// markdown file without a `type:` key to a plain note, not a failed instance.
/// Both audiences are served: a direct `parse_instance` caller sees the
/// diagnostic; the build keeps notes-with-frontmatter quiet.
pub const MISSING_TYPE_CLAIM: DiagnosticCode = DiagnosticCode::from_static("missing-type-claim");

/// Instance `type:` is malformed: not a string or list of strings, an
/// empty list (`type: []` must name at least one type), or a non-string
/// list element. Spec [[type list form::au-type-system]], [[type-instance type::au-type-system]].
pub const INSTANCE_CLAIM_BAD_SHAPE: DiagnosticCode =
    DiagnosticCode::from_static("instance-claim-bad-shape");

/// `fields:`, `sealed:`, or `meta:` appears as a key on an instance —
/// these are type-def-only keys per [[type-def::au-type-system]].
pub const RESERVED_KEY_ON_INSTANCE: DiagnosticCode =
    DiagnosticCode::from_static("reserved-key-on-instance");

/// `^:` value on an inline record fails the block-id grammar
/// (`[A-Za-z0-9_-]+`) or is not a scalar. The body parser skips a
/// malformed `^marker` silently — it reads as prose; an explicit `^:`
/// key cannot be prose, so it surfaces. Spec [[type block-id::au-type-system]].
pub const BLOCK_ID_MALFORMED: DiagnosticCode = DiagnosticCode::from_static("block-id-malformed");

/// `^:` at the top level of instance frontmatter — the file is
/// addressable by name, block-ids belong on inline records. The key is
/// dropped with a warning, never silently meaningful. Spec [[type block-id::au-type-system]].
pub const BLOCK_ID_ON_INSTANCE_ROOT: DiagnosticCode =
    DiagnosticCode::from_static("block-id-on-instance-root");

/// A navigational wikilink whose target resolves to no repo file.
/// Fires for a bare body wikilink, a wikilink embedded in a frontmatter
/// value, and a `[[...]]` in a `#:` docstring alike — navigational sites are
/// open-world growth, the target may be authored next; the link is surfaced
/// as dangling, never rejected. Slot-side validated references fire
/// `reference-target-missing`, also a warning; the two differ only by code,
/// navigational versus validated.
pub const NAVIGATIONAL_TARGET_NOT_FOUND: DiagnosticCode =
    DiagnosticCode::from_static("navigational-target-not-found");

/// A navigational wikilink whose target resolves to two or more repo files,
/// so it lands on too much rather than nothing. Fires for a bare body
/// wikilink, a wikilink embedded in a frontmatter value, and a `[[...]]` in a
/// `#:` docstring alike — an extensionless target matching by stem is the
/// common cause (`note.md` + `note.yaml`). Navigational sites are open-world growth, so it is a
/// warning, the same stance as `navigational-target-not-found`. The
/// slot-side validated twin fires `reference-target-ambiguous`, an error;
/// the two differ by severity and code, navigational versus validated.
pub const NAVIGATIONAL_TARGET_AMBIGUOUS: DiagnosticCode =
    DiagnosticCode::from_static("navigational-target-ambiguous");

/// A navigational wikilink whose `^block-id` exists on neither of the
/// target's addressable surfaces ([[type block-id::au-type-system]]) — no record `^:`
/// id, no body marker or fence id. Same severity stance as
/// `navigational-target-not-found`: growth is the same story in body
/// prose, inside frontmatter values, and in a `#:` docstring. Local forms
/// (`[[^id]]`) included.
pub const NAVIGATIONAL_BLOCK_ID_NOT_FOUND: DiagnosticCode =
    DiagnosticCode::from_static("navigational-block-id-not-found");

/// A wikilink's `#head` fragment matches no heading in the resolved
/// target, per the [[type reference::au-type-system]] anchor-matching contract. Fires
/// for prose, slot values, and `#:` docstrings alike — anchors are
/// navigational everywhere, so the severity is warning everywhere. Silent
/// when the engine doesn't hold the target's body (plain notes, assets).
pub const ANCHOR_NOT_FOUND: DiagnosticCode = DiagnosticCode::from_static("anchor-not-found");

/// A YAML mapping key at the top level of an instance or type-def was
/// not a string (e.g. `1: foo` or `[a, b]: foo`). The engine cannot
/// honor non-string keys, so the entry is dropped — Severity::Warning
/// surfaces the likely authoring mistake instead of letting it slip
/// silently.
pub const MAPPING_KEY_NOT_A_STRING: DiagnosticCode =
    DiagnosticCode::from_static("mapping-key-not-a-string");

/// A type-def top-level key was none of `extends` / `fields` / `sealed` /
/// `abstract` / `meta` / `body`. Likely a typo (e.g. `feilds:`); could also be
/// a custom extension that V1's closed schema doesn't honor. A stray `type:` is
/// the exception: it fires `type-key-on-type-def` instead, so a mistaken parent
/// claim is never silently dropped. Severity::Warning — the engine drops the
/// entry, the user sees what they typed.
pub const UNKNOWN_TOP_LEVEL_KEY: DiagnosticCode =
    DiagnosticCode::from_static("unknown-top-level-key");

/// A `#:` doc comment binds to no declaration, so it is dropped. Advisory:
/// `#:` signals intended documentation, not an incidental `#` comment. See
/// [[type docstring::au-type-system]].
pub const DANGLING_DOC_COMMENT: DiagnosticCode =
    DiagnosticCode::from_static("dangling-doc-comment");

/// A `location:` block on a type-def is structurally malformed: not a mapping,
/// a non-string `name`/`path`, a `fileType` that is not `md`/`yaml`, a non-bool
/// `strict`, an unknown sub-key, an unterminated `${` name template, or a bad
/// path glob (absolute, `..`, an empty or partial-glob segment). An `error`; the
/// field-safety and fileType/body checks that need the type's own fields are a
/// separate load pass. See
/// [[spec - location constraints - a name template and path predicate as an advisory placement meet]].
pub const LOCATION_BAD_SHAPE: DiagnosticCode = DiagnosticCode::from_static("location-bad-shape");

/// A type-def declares `location.fileType: yaml` beside a non-empty `body:`.
/// A non-empty body forces markdown, `fileType: yaml` forces yaml, so every
/// instance is guaranteed a diagnostic: the type is unsatisfiable. An `error`.
/// See [[spec - location constraints - a name template and path predicate as an advisory placement meet]].
pub const LOCATION_FILETYPE_BODY_CONFLICT: DiagnosticCode =
    DiagnosticCode::from_static("location-filetype-body-conflict");

/// An instance's placement does not satisfy a SOFT (non-strict) location it is
/// subject to, and it matches no claimed location. A `warning`, advisory, the
/// same tier as `readme-misplaced`. A single-claim mismatch and a mixin
/// matching none of its claims are the one case. See
/// [[spec - location constraints - a name template and path predicate as an advisory placement meet]].
pub const LOCATION_MISMATCH: DiagnosticCode = DiagnosticCode::from_static("location-mismatch");

/// In a mixin, the instance matched at least one claimed location but not this
/// SOFT one. A `hint`, the lowest tier: a file cannot be in two places, so an
/// unmet sibling is not a violation. See
/// [[spec - location constraints - a name template and path predicate as an advisory placement meet]].
pub const LOCATION_PARTIAL_UNMET: DiagnosticCode =
    DiagnosticCode::from_static("location-partial-unmet");

/// An instance does not satisfy a `strict: true` location. A strict location is
/// mandatory, so it opts out of the mixin satisfy-any, an `error`. See
/// [[spec - location constraints - a name template and path predicate as an advisory placement meet]].
pub const LOCATION_STRICT_VIOLATION: DiagnosticCode =
    DiagnosticCode::from_static("location-strict-violation");

// ----- instance validation -----

/// Instance `type:` claim names a type-def absent from the type graph.
pub const UNKNOWN_TYPE_CLAIM: DiagnosticCode = DiagnosticCode::from_static("unknown-type-claim");

/// A required (non-optional) field declared by the closure is missing on the
/// instance. When an undeclared present key is a near-miss (small edit
/// distance) of the missing name, the message appends a "did you mean '{key}'?"
/// hint plus a related span at the suspected key; advisory, the code and
/// severity are unchanged.
pub const REQUIRED_FIELD_ABSENT: DiagnosticCode =
    DiagnosticCode::from_static("required-field-absent");

/// A field's value doesn't match the shape declared by its type-def.
pub const FIELD_SHAPE_MISMATCH: DiagnosticCode =
    DiagnosticCode::from_static("field-shape-mismatch");

/// A `Number` field holds a non-finite float (`.nan`, `.inf`, `-.inf`). These
/// parse as floats but are not valid numbers. Distinct from
/// `field-shape-mismatch` so the message names the real problem rather than
/// claiming the value is not a number.
pub const NON_FINITE_NUMBER: DiagnosticCode = DiagnosticCode::from_static("non-finite-number");

/// A value is the right base type but falls outside its slot's value refinement
/// ([[type-def field shape::au-type-system]], `Base{predicate}`) — e.g. `-1` in a
/// `Number{>=0}` slot. Distinct from `field-shape-mismatch` so the message
/// names the failed predicate rather than claiming a type mismatch. Suppressed
/// when the refinement is provably empty (that fires `refinement-unsatisfiable`
/// on the type-def instead).
pub const VALUE_OUT_OF_REFINEMENT: DiagnosticCode =
    DiagnosticCode::from_static("value-out-of-refinement");

/// A type-def declares a value refinement ([[type-def field shape::au-type-system]]) whose
/// numeric meet admits no value — `Number{>=5 & <=1}`, `Number{>2 & <3 &
/// integer}`. A `Warning`-severity advisory on the type-def: the slot can never
/// be satisfied, so every value would fail; the validator suppresses the
/// per-value `value-out-of-refinement` and surfaces this once instead.
pub const REFINEMENT_UNSATISFIABLE: DiagnosticCode =
    DiagnosticCode::from_static("refinement-unsatisfiable");

/// Same field name reached by mixin ancestors with non-token-equal
/// `parsed_shape` (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]). Also covers the cross-repo same-name
/// diamond: a folded mixin reaching two distinct identities of one type name
/// (own `note` plus peer `note::base`, or two peers) that disagree on a field.
/// The origin's authored form (`note::base`) distinguishes the identities in the
/// message. Fires at validate time on a BARE use of the divergent field; an
/// untouched divergent field is clean, and a divergent inherited field is legal
/// at a type-def (the type-graph load fires nothing). Severity::Error. The
/// divergent field stays in the effective shape (in `EffectiveShape.divergent`),
/// resolvable per-instance by a `field{type}` key.
pub const MIXIN_COLLISION: DiagnosticCode = DiagnosticCode::from_static("mixin-collision");

/// Repeated name in a `type:` list (spec [[type-instance type::au-type-system]] redundant-claims rule).
/// Severity::Warning — the closure dedupes silently; the warning surfaces
/// the likely authoring mistake. Fires at both load time (type-def parents)
/// and validate time (instance claims).
pub const DUPLICATE_CLAIM: DiagnosticCode = DiagnosticCode::from_static("duplicate-claim");

/// One claim's closure includes another in the same `type:` list — the
/// wider claim is implied by the narrower (spec [[type-instance type::au-type-system]] — symmetric to
/// [[type-def shape compound::au-type-system]]'s slot-union subsumption rule). Severity::Warning. Fires at both
/// load time and validate time.
pub const SUBSUMPTION_IN_MIXIN: DiagnosticCode =
    DiagnosticCode::from_static("subsumption-in-mixin");

/// One reference branch in a slot-union is a subtype of another (spec [[type-def shape compound::au-type-system]]).
/// E.g. `<decision | decision.decided>` — `decision.decided`'s closure
/// includes `decision`, so `decision.decided` is the narrower branch. In
/// a union, the wider branch subsumes the narrower, so the *narrower* is
/// the redundant branch (the wider already accepts every value it would).
/// Severity::Warning. Symmetric in shape (not in flagged direction) to
/// `subsumption-in-slot-intersection`. Fires only on reference branches
/// with closures in the graph; primitives and enums in a union are silent
/// (no closure to compare).
pub const SUBSUMPTION_IN_SLOT_UNION: DiagnosticCode =
    DiagnosticCode::from_static("subsumption-in-slot-union");

/// One reference branch in a slot-intersection is a subtype of another
/// (spec [[type-def shape compound::au-type-system]]). E.g. `<decision & decision.decided>` collapses to
/// `decision.decided` — the *wider* `decision` branch is redundant
/// because the narrower already implies it. Severity::Warning. Mirror in
/// shape of `subsumption-in-slot-union`; same pairwise closure-inclusion
/// check, but the redundant-direction flips (intersection narrows;
/// union widens).
pub const SUBSUMPTION_IN_SLOT_INTERSECTION: DiagnosticCode =
    DiagnosticCode::from_static("subsumption-in-slot-intersection");

/// Qualified key `field{type}` is syntactically broken (empty field, empty
/// qualifier, unclosed or stray braces, qualifier type violates the
/// type-name regex). Spec [[type-def fields collision - auto-unify and qualified field::au-type-system]].
pub const MALFORMED_QUALIFIER_KEY: DiagnosticCode =
    DiagnosticCode::from_static("malformed-qualifier-key");

/// Qualified key `field{type}` names a `type` that is not in the instance's
/// effective closure (its claims plus all transitive ancestors). Spec [[type-def fields collision - auto-unify and qualified field::au-type-system]].
pub const QUALIFIER_NOT_IN_CLOSURE: DiagnosticCode =
    DiagnosticCode::from_static("qualifier-not-in-closure");

/// Qualified key `field{type}` names a `type` that IS in the instance's
/// closure, but no type-def in `closure_of(type)` literally lists `field`
/// in its `fields:`. Spec [[type-def fields collision - auto-unify and qualified field::au-type-system]].
pub const QUALIFIER_DOES_NOT_DECLARE_FIELD: DiagnosticCode =
    DiagnosticCode::from_static("qualifier-does-not-declare-field");

/// Qualified key `field{type}` names a `type` whose closure declares `field`
/// at two or more NON-token-equal origins — a divergent field reached through a
/// non-declaring descendant. Which origin's shape the value would check against
/// is arbitrary, so the qualifier is rejected; name a declaring origin directly
/// (`field{origin}`). Spec [[type-def fields collision - auto-unify and qualified field::au-type-system]].
pub const QUALIFIER_AMBIGUOUS: DiagnosticCode = DiagnosticCode::from_static("qualifier-ambiguous");

/// Same field name on one instance has both bare and qualified entries
/// (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]). All uses of a single field name must agree on form —
/// either every use is bare (auto-unify governs) or every use is
/// qualified (explicit per-origin distinction). Mixing the two is a
/// contradictory claim about whether the field auto-unified.
pub const MIXED_BARE_AND_QUALIFIED_FIELD: DiagnosticCode =
    DiagnosticCode::from_static("mixed-bare-and-qualified-field");

/// Instance `type:` claim names a sealed type-def directly
/// (spec [[type-def sealed::au-type-system]]). Per-claim: in a `type:` list, each element is checked
/// independently and a non-sealed sibling does not excuse a sealed
/// claim. Same code whether the sealed name is a top-level sealed
/// parent or a sealed intermediate in a nested sum.
pub const SEALED_PARENT_CLAIMED: DiagnosticCode =
    DiagnosticCode::from_static("sealed-parent-claimed");

/// Instance `type:` claim list contains two or more distinct non-sealed
/// leaves descending from the same sealed type-def (spec [[type-instance type::au-type-system]]).
/// A sealed family is a discriminated union — a single file cannot
/// simultaneously be two of its sibling leaves. Fires once per violated
/// sealed family, naming the innermost-shared sealed ancestor when
/// nested sums overlap. Applies symmetrically to inline-value identity
/// claims.
pub const MULTI_LEAF_IN_SEALED_FAMILY: DiagnosticCode =
    DiagnosticCode::from_static("multi-leaf-in-sealed-family");

/// Inline value's declared `type:` exists in the graph but does not
/// satisfy the slot's demand (spec [[type-def shape record::au-type-system]]). Trigger sites: case 1
/// declared closure missing the slot's demanded type; case 3 declared
/// not in any union branch; case 4 declared closure missing one or more
/// intersection branches; a qualified `foo::repo` demand whose inline
/// claim's FOLDED closure does not include the demanded peer type. One
/// code, message variant per case.
pub const INLINE_VALUE_TYPE_NOT_COMPATIBLE: DiagnosticCode =
    DiagnosticCode::from_static("inline-value-type-not-compatible");

/// Inline value at a slot that requires an explicit `type:` claim
/// (spec [[type-def shape record::au-type-system]] cases 2/3/4 — sealed-parent slot, union slot, or
/// intersection slot, plus a sealed peer `foo::repo` demand) omits the
/// claim. Single code, message variant per case.
pub const INLINE_VALUE_MISSING_TYPE: DiagnosticCode =
    DiagnosticCode::from_static("inline-value-missing-type");

// ----- body typing v5 ([[type-def body::au-type-system]], [[type-def body use::au-type-system]], [[type-def body fills::au-type-system]]) -----

/// `body:` value is not a YAML list. Spec [[type-def body::au-type-system]].
pub const BODY_NOT_A_LIST: DiagnosticCode = DiagnosticCode::from_static("body-not-a-list");

/// `body:` item is not a YAML mapping. Each item must be a map carrying
/// a discriminating key (`use:`, `section:`, `section?:`, `fills:`, or
/// `fills!:`). Spec [[type-def body::au-type-system]].
pub const BODY_ITEM_NOT_A_MAPPING: DiagnosticCode =
    DiagnosticCode::from_static("body-item-not-a-mapping");

/// `body:` item lacks any of the recognized discriminator keys.
/// Spec [[type-def body::au-type-system]].
pub const BODY_ITEM_MISSING_DISCRIMINATOR: DiagnosticCode =
    DiagnosticCode::from_static("body-item-missing-discriminator");

/// `fills:` value is not a field name or list of field names. Spec [[type-def body fills::au-type-system]].
pub const FILLS_VALUE_BAD_SHAPE: DiagnosticCode =
    DiagnosticCode::from_static("fills-value-bad-shape");

/// `fills:` and `fills!:` declared on the same scope. Spec [[type-def body fills::au-type-system]].
pub const FILLS_DOUBLE_FORM_DECLARATION: DiagnosticCode =
    DiagnosticCode::from_static("fills-double-form-declaration");

/// `use:` of a type-def outside the current type-def's closure. Also fires
/// cross-repo: a `use: parent::repo` whose peer is not in the host's FOLDED
/// closure (the host does not extend `parent::repo`), checked in the engine's
/// cross-repo gate. Spec [[type-def body use::au-type-system]].
pub const BODY_USE_OUT_OF_CLOSURE: DiagnosticCode =
    DiagnosticCode::from_static("body-use-out-of-closure");

/// `use:` splice graph contains a cycle. Spec [[type-def body use::au-type-system]].
pub const BODY_USE_CYCLE: DiagnosticCode = DiagnosticCode::from_static("body-use-cycle");

/// `use:` appears inside a nested `body:` (only allowed at the top of the
/// outermost body). Spec [[type-def body use::au-type-system]]. Not enumerated in the source spec; added during
/// implementation.
pub const BODY_USE_NESTED: DiagnosticCode = DiagnosticCode::from_static("body-use-nested");

/// `use: T` resolves to a type-def `T` that declares no `body:` (or
/// declares `body: []`) — the splice contributes nothing, so the line
/// is dead syntax. Severity: warning. The instance still validates;
/// surfaced because it's almost always a typo on the target name or a
/// dangling `use:` left after the target lost its body. Also fires cross-repo
/// for an in-closure `use: parent::repo` whose peer type declares no body. Spec [[type-def body use::au-type-system]].
pub const BODY_USE_TARGET_HAS_NO_BODY: DiagnosticCode =
    DiagnosticCode::from_static("body-use-target-has-no-body");

/// `fills:` references a field absent from the type-def's effective
/// closure. Spec [[type-def body fills::au-type-system]].
pub const FILLS_UNKNOWN_FIELD: DiagnosticCode = DiagnosticCode::from_static("fills-unknown-field");

/// Nested `fills:` requires a field forbidden by an ancestor `fills!:`.
/// Spec [[type-def body fills::au-type-system]].
pub const FILLS_CONTRACT_CONFLICT_NESTED_EXCLUSIVITY: DiagnosticCode =
    DiagnosticCode::from_static("fills-contract-conflict-nested-exclusivity");

// `body:` on instance frontmatter reuses RESERVED_KEY_ON_INSTANCE (parameterized
// message names the offending key); no dedicated code.

// ----- body typing v5 — per-instance validation ([[type-def body section::au-type-system]], [[type-def body fills::au-type-system]], [[type-def body::au-type-system]]) -----

/// Yaml-only instance of a type-def that declares a non-empty `body:`
/// template. Spec [[type-def body::au-type-system]].
pub const BODY_REQUIRED_BUT_YAML_ONLY_INSTANCE: DiagnosticCode =
    DiagnosticCode::from_static("body-required-but-yaml-only-instance");

/// Declared body section absent from the instance's prose.
/// Spec [[type-def body section::au-type-system]].
pub const BODY_SECTION_MISSING: DiagnosticCode =
    DiagnosticCode::from_static("body-section-missing");

/// Declared body section appears out of its templated position relative
/// to a sibling declared section. Spec [[type-def body section::au-type-system]] ("position in the ordering
/// must be respected").
pub const BODY_SECTION_OUT_OF_ORDER: DiagnosticCode =
    DiagnosticCode::from_static("body-section-out-of-order");

/// Fenced code block opens but no closing fence of at least the open's
/// backtick-run length appears in the rest of the source. Fences are
/// variable-length per CommonMark, so a longer fence wraps a shorter one
/// (a quad-backtick fence around a triple-backtick example does not
/// trigger this). The parser emits a recovery event and continues
/// scanning subsequent lines as ordinary content; this diagnostic
/// surfaces the unterminated open. Advisory severity per CommonMark
/// (which permits unterminated fences but reads them ambiguously).
/// Spec [[type-instance body contribution::au-type-system]].
pub const BODY_UNTERMINATED_FENCE: DiagnosticCode =
    DiagnosticCode::from_static("body-unterminated-fence");

/// Two or more blocks in a single file share the same `^block-id`.
/// `resolve_block_id` returns the first match, silently ignoring later
/// ones; this diagnostic surfaces the duplication so authors can
/// disambiguate. Spec [[type block-id::au-type-system]].
pub const BLOCK_ID_DUPLICATE: DiagnosticCode = DiagnosticCode::from_static("block-id-duplicate");

/// Scope declared `fills: f` but contains no contribution to f.
/// Spec [[type-def body fills::au-type-system]].
pub const FILLS_CONTRACT_UNMET: DiagnosticCode =
    DiagnosticCode::from_static("fills-contract-unmet");

/// Scope declared `fills!: f` but contains a contribution to a different
/// field. Spec [[type-def body fills::au-type-system]].
pub const FILLS_CONTRACT_EXCEEDED: DiagnosticCode =
    DiagnosticCode::from_static("fills-contract-exceeded");

/// Body contributes to a field whose frontmatter key is absent.
/// Spec [[type-instance body contribution::au-type-system]].
pub const BODY_FILLS_WITHOUT_FRONTMATTER_KEY: DiagnosticCode =
    DiagnosticCode::from_static("body-fills-without-frontmatter-key");

/// Effective ValueContainer list exceeds the field's declared cardinality.
/// Spec [[type value container::au-type-system]].
pub const FIELD_CARDINALITY_EXCEEDED: DiagnosticCode =
    DiagnosticCode::from_static("field-cardinality-exceeded");

/// `[[target:field]]` references a field absent from the instance's
/// effective closure. Spec [[type-instance body contribution::au-type-system]].
pub const UNBOUND_FIELD_BINDING: DiagnosticCode =
    DiagnosticCode::from_static("unbound-field-binding");

/// Inline `[:field]` references a field absent from the closure (advisory).
/// Spec [[type-instance body contribution::au-type-system]].
pub const UNKNOWN_FIELD_IN_PROSE_CONTRIBUTION: DiagnosticCode =
    DiagnosticCode::from_static("unknown-field-in-prose-contribution");

/// Body contribution's value doesn't satisfy the bound field's shape.
/// Spec [[type-instance body contribution::au-type-system]].
pub const BODY_SLOT_SHAPE_MISMATCH: DiagnosticCode =
    DiagnosticCode::from_static("body-slot-shape-mismatch");

/// Inline code starts with `[:` but doesn't match `[:fieldName] value`.
/// Severity: warning — `[:field]` and similar near-misses ARE reserved
/// attribution syntax, but the author's intent is ambiguous (bare
/// `[:field]` is a common documentation idiom). Spec [[type-instance body contribution::au-type-system]].
pub const MALFORMED_ATTRIBUTION_MARKER: DiagnosticCode =
    DiagnosticCode::from_static("malformed-attribution-marker");

/// A `^^id` block-referent resolves to a plain (fence-less) block. Fires only on
/// the `^^` path; a bare `^id` is navigational. Spec [[type block-id::au-type-system]].
pub const BLOCK_ID_NOT_TYPED: DiagnosticCode = DiagnosticCode::from_static("block-id-not-typed");

/// A `^^id` block-referent's id is absent from the target file. Fires only on the
/// `^^` path; a bare `^id`'s missing id is `navigational-block-id-not-found`
/// (warning). Spec [[type block-id::au-type-system]].
pub const BLOCK_ID_NOT_FOUND: DiagnosticCode = DiagnosticCode::from_static("block-id-not-found");

/// A marked fence reading as a record (`type: T`) fails T's contract.
/// Spec [[type-instance body contribution::au-type-system]].
pub const EMBEDDED_RECORD_VALIDATION_FAILURE: DiagnosticCode =
    DiagnosticCode::from_static("embedded-record-validation-failure");

/// A marked fence at a compound slot admitting BOTH a record and a text branch
/// read as the RECORD branch, because its content parses as a mapping declaring
/// `type:`, and that record then failed. Names the cause the accompanying
/// failure cannot: the author may have meant structured key-value TEXT, which
/// happens to be a well-formed typed mapping. Narrow, the whole fence must parse
/// as yaml, so ordinary prose never reaches it. A `hint`, riding ALONGSIDE that
/// failure rather
/// than replacing it — the record really is invalid, so the error keeps its own
/// severity. A fence whose record reads and validates fires nothing.
/// Spec [[type-instance body contribution::au-type-system]].
pub const BODY_FENCE_READ_AS_RECORD: DiagnosticCode =
    DiagnosticCode::from_static("body-fence-read-as-record");
