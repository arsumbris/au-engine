//! Canonical type-def identity: the `(name, hash)` hash half.
//!
//! The hash is over a deterministic, normalized serialization of a `TypeDef`'s
//! own declaration, scoped to what shapes instance validation. Two defs with
//! the same name and the same canonical form are the same type and unify;
//! diverging forms are a drift.
//!
//! Normalization:
//! - in, order-normalized (sorted): own fields (name + optional + canonical
//!   shape), parent-claim names, sealed leaf names.
//! - in, order-preserved: enum value lists (inside the shape), body items
//!   (sections with their `guidance`, fills, and nested bodies). The body is
//!   the type's authoring surface, so it counts wholesale.
//! - in, APPEND-ONLY-WHEN-PRESENT: the `abstract: true` marker and the
//!   `required:` meta obligations. Both are contract-shaped (abstract removes the
//!   type from the valid-instance-set, `required:` narrows the valid-subtype
//!   contract), so they fold into identity, unlike descriptive meta value blocks.
//!   Each segment is emitted ONLY when present, so a plain type's form is
//!   byte-identical to before the segments existed and no existing `(name, hash)`
//!   identity rotates. See
//!   [[spec - abstract type-defs - a non-claimable open type-def, sealed is abstract plus closed]]
//!   and [[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]].
//! - out: the type name (paired beside the hash), comments, `#:` docstrings
//!   (advisory, see [[type docstring::au-type-system]]), key order, whitespace, meta (separate
//!   type-level metadata), and all spans.
//!
//! Free text (a section's heading and its `guidance`) is hashed BYTE-EXACT, with
//! no Unicode normalization, so two canonically-equivalent strings that differ
//! in bytes hash differently. Identifiers (field / type / sealed names) are
//! ASCII-constrained upstream, so this only affects free text. The contract is
//! byte-exact, not Unicode-canonical.
//!
//! The canonical form IS the stability contract. Changing what it includes
//! changes every hash, which re-flags cross-repo drift across all copies. The
//! hash function is FNV-1a 64-bit, matching the engine's other provisional
//! content hash; it is swappable without changing the form.

use crate::body::{BodyItem, FillsContract};
use crate::typedef::{FieldDecl, TypeDef};

/// FNV-1a 64-bit fingerprint of a type-def's canonical form.
///
/// Identity is the pair `(name, CanonicalHash)`: the name travels beside the
/// hash, it is not part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalHash(pub u64);

impl CanonicalHash {
    pub fn of(td: &TypeDef) -> Self {
        Self(fnv1a(canonical_form(td).as_bytes()))
    }
}

/// FNV-1a 64-bit over `bytes`. The engine's provisional content-hash function,
/// shared by the def-local [`CanonicalHash`] and the referenced-closure
/// `ClosureHash` ([`crate::closure_id`]) so both rest on one swappable primitive.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Length-prefix a string so a delimiter inside it can never be mistaken for a
/// structural separator. `len:bytes`.
pub(crate) fn lp(out: &mut String, s: &str) {
    out.push_str(&s.len().to_string());
    out.push(':');
    out.push_str(s);
}

/// The deterministic, normalized serialization a [`CanonicalHash`] hashes.
///
/// Exposed so a comparison can be exact (no hash collision) where that matters;
/// the hash is the compact fingerprint of this string.
pub fn canonical_form(td: &TypeDef) -> String {
    let mut out = String::from("td;");

    // Parents: claim names with their `::repo` qualifier, sorted and deduped.
    // Order is non-semantic, the closure dedupes. Parent content is hashed
    // separately via the parent's own identity. The `::repo` qualifier is
    // included so a peer parent `extends: T::repo` never hashes identical to an own
    // `extends: T` (mirroring the `use:` arm); a same-named parent in a different
    // repo is a different type.
    let mut parents: Vec<String> = td
        .parents
        .iter()
        .map(|p| match &p.repo {
            Some(r) => format!("{}::{}", p.name.as_str(), r),
            None => p.name.as_str().to_string(),
        })
        .collect();
    parents.sort_unstable();
    parents.dedup();
    out.push('P');
    out.push_str(&parents.len().to_string());
    out.push(';');
    for p in &parents {
        lp(&mut out, p);
    }

    // Sealed leaves: a set, sorted and deduped.
    let mut sealed: Vec<&str> = td.sealed.iter().map(|s| s.name.as_str()).collect();
    sealed.sort_unstable();
    sealed.dedup();
    out.push('S');
    out.push_str(&sealed.len().to_string());
    out.push(';');
    for s in sealed {
        lp(&mut out, s);
    }

    // Fields: sorted by (name, optional, shape) so declaration order does not
    // matter. The shape is the canonical render, so formatting and whitespace
    // do not matter; enum value order is preserved inside it.
    let mut fields: Vec<(&str, bool, String)> = td
        .fields
        .iter()
        .map(|f: &FieldDecl| (f.name.as_str(), f.optional, f.shape_display()))
        .collect();
    fields.sort();
    out.push('F');
    out.push_str(&fields.len().to_string());
    out.push(';');
    for (name, optional, shape) in &fields {
        lp(&mut out, name);
        out.push(if *optional { '1' } else { '0' });
        lp(&mut out, shape);
    }

    // Body: order-preserved. `body: []` and an absent `body:` both serialize
    // to just the tag, matching their identical behaviour ([[type-def body::au-type-system]]).
    out.push('B');
    match &td.body {
        Some(body) if !body.is_empty() => canonical_body(body, &mut out),
        _ => {}
    }

    // Abstract: APPEND-ONLY-WHEN-DECLARED, at the very end. Emitting the `A`
    // marker only for an abstract type keeps a concrete type's canonical string
    // byte-identical, so no existing identity rotates (protecting the SDK
    // hardwired-hash mirror). `abstract` is contract-shaped, it narrows the
    // valid-instance-set, so two same-name defs differing only in it are
    // distinct identities. See [[spec - abstract type-defs - a non-claimable open type-def, sealed is abstract plus closed]].
    if td.declared_abstract {
        out.push('A');
    }

    // Required-meta obligations: APPEND-ONLY-WHEN-PRESENT, after the abstract
    // marker, sorted authored forms (with the `::repo` qualifier). The obligation
    // is contract-shaped, it narrows the valid-subtype contract, so it folds into
    // identity; descriptive meta value blocks stay excluded. A def with no
    // `required:` emits nothing here, so its canonical string is unchanged and no
    // existing identity rotates. See
    // [[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]].
    if !td.required_meta.is_empty() {
        let mut req: Vec<String> = td
            .required_meta
            .iter()
            .map(|r| match &r.repo {
                Some(rr) => format!("{}::{}", r.name.as_str(), rr),
                None => r.name.as_str().to_string(),
            })
            .collect();
        req.sort_unstable();
        req.dedup();
        out.push('R');
        out.push_str(&req.len().to_string());
        out.push(';');
        for r in &req {
            lp(&mut out, r);
        }
    }

    // Brand shape: APPEND-ONLY-WHEN-PRESENT, at the very end. A brand declares
    // `shape:` instead of `fields:`, naming an underlying scalar, named enum,
    // named union, or tuple. The shape's canonical render folds in, so enum
    // member order is significant and a shape change rotates identity; the
    // per-member docstrings are a separate field, excluded like every docstring.
    // Emitted only for a brand, so a record's canonical string is unchanged and
    // no existing identity rotates. The shape is contract-shaped, it defines the
    // valid-value set. See
    // [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
    if let Some(brand) = &td.shape {
        out.push('H');
        lp(&mut out, &brand.shape.to_string());
    }

    out
}

fn canonical_body(items: &[BodyItem], out: &mut String) {
    out.push('[');
    out.push_str(&items.len().to_string());
    out.push(';');
    for item in items {
        match item {
            // A `use:` is hashed as the literal marker, NOT its spliced
            // contribution. The hash is def-local, and `use: T` is closure-gated
            // (`body-use-out-of-closure`), so T is always an ancestor carrying
            // its own `(name, hash)`, exactly like a parent's fields. Cross-repo
            // reference sameness is established by a separate full-closure
            // comparison, not this hash.
            BodyItem::Use {
                type_name, repo, ..
            } => {
                out.push('u');
                // Include the `::repo` qualifier so a peer `use: T::repo` never
                // hashes identical to an own `use: T`.
                match repo {
                    Some(r) => lp(out, &format!("{}::{}", type_name.as_str(), r)),
                    None => lp(out, type_name.as_str()),
                }
            }
            BodyItem::Section {
                name,
                optional,
                guidance,
                fills,
                body,
                // spans, source_path excluded.
                ..
            } => {
                out.push('s');
                lp(out, name);
                out.push(if *optional { '1' } else { '0' });
                match guidance {
                    None => out.push('_'),
                    Some(g) => {
                        out.push('g');
                        lp(out, g);
                    }
                }
                canonical_fills(fills.as_ref(), out);
                match body {
                    Some(b) => canonical_body(b, out),
                    None => out.push_str("[0;"),
                }
            }
            BodyItem::Fills { contract, .. } => {
                out.push('f');
                canonical_fills(Some(contract), out);
            }
        }
    }
}

fn canonical_fills(contract: Option<&FillsContract>, out: &mut String) {
    match contract {
        None => out.push('_'),
        Some(c) => {
            out.push(if c.exclusive { '!' } else { '.' });
            // Field set, order non-semantic, sorted.
            let mut names: Vec<&str> = c.fields.iter().map(|x| x.name.as_str()).collect();
            names.sort_unstable();
            out.push_str(&names.len().to_string());
            out.push(';');
            for n in names {
                lp(out, n);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typedef::parse_type_def;
    use au_parser::yaml::parse;
    use std::path::Path;

    fn td(source: &str, path: &str) -> TypeDef {
        let docs = parse(source).unwrap();
        parse_type_def(Path::new(path), source, 0, &docs[0])
            .type_def
            .expect("parses")
    }

    fn hash(source: &str, path: &str) -> u64 {
        CanonicalHash::of(&td(source, path)).0
    }

    #[test]
    fn location_is_excluded_from_identity() {
        // `location:` is advisory placement, not the value contract, so a copy
        // that adds or changes it stays in sync with its location-less peer. This
        // guards the decision that placement never participates in type identity.
        let plain = "fields:\n  a: String\n";
        let located = "fields:\n  a: String\nlocation:\n  name: \"${.type}\"\n  strict: true\n";
        assert_eq!(
            hash(plain, "/v/note.type.yaml"),
            hash(located, "/v/note.type.yaml"),
        );
    }

    #[test]
    fn brand_enum_member_order_is_significant() {
        assert_ne!(
            hash("shape:\n  - a\n  - b\n", "/v/e.type.yaml"),
            hash("shape:\n  - b\n  - a\n", "/v/e.type.yaml"),
        );
    }

    #[test]
    fn brand_member_docs_do_not_rotate_identity() {
        // Docstrings are advisory, excluded from the hash.
        assert_eq!(
            hash(
                "shape:\n  - a   #: doc a\n  - b   #: doc b\n",
                "/v/e.type.yaml"
            ),
            hash("shape:\n  - a\n  - b\n", "/v/e.type.yaml"),
        );
    }

    #[test]
    fn a_brand_and_a_record_are_distinct_identities() {
        assert_ne!(
            hash("shape: Number\n", "/v/x.type.yaml"),
            hash("fields:\n  a: Number\n", "/v/x.type.yaml"),
        );
    }

    #[test]
    fn a_tuple_brand_folds_its_shape_into_identity() {
        // A tuple brand's element shapes and their order fold into identity.
        assert_ne!(
            hash("shape: (Number, Number)\n", "/v/pt.type.yaml"),
            hash("shape: (Number, String)\n", "/v/pt.type.yaml"),
            "a different element shape is a different identity",
        );
        assert_ne!(
            hash("shape: (String, Number)\n", "/v/pt.type.yaml"),
            hash("shape: (Number, String)\n", "/v/pt.type.yaml"),
            "element order is significant",
        );
        assert_ne!(
            hash("shape: (Number, Number)\n", "/v/pt.type.yaml"),
            hash("shape: (Number, Number, Number)\n", "/v/pt.type.yaml"),
            "arity is part of identity",
        );
    }

    #[test]
    fn a_brand_shape_change_rotates_identity() {
        assert_ne!(
            hash("shape: Number\n", "/v/x.type.yaml"),
            hash("shape: String\n", "/v/x.type.yaml"),
        );
    }

    #[test]
    fn a_refined_scalar_brand_folds_its_refinement_into_identity() {
        // `percent := shape: Number{>=0 & <=100}`. The refinement is part of the
        // brand's shape rendering, so it folds into the `H` segment: a refined
        // brand is distinct from its unrefined base and from a different bound.
        let percent = "shape: Number{>=0 & <=100}\n";
        assert_ne!(
            hash(percent, "/v/percent.type.yaml"),
            hash("shape: Number\n", "/v/percent.type.yaml"),
            "a refined brand differs from the bare scalar brand",
        );
        assert_ne!(
            hash(percent, "/v/percent.type.yaml"),
            hash("shape: Number{>=0 & <=1}\n", "/v/percent.type.yaml"),
            "a different bound is a different identity",
        );
        // Stable across builds: re-hashing the same source is byte-identical.
        assert_eq!(
            hash(percent, "/v/percent.type.yaml"),
            hash(percent, "/v/percent.type.yaml"),
        );
    }

    #[test]
    fn a_record_canonical_form_carries_no_brand_segment() {
        // The brand segment is append-only-when-present, so a record's canonical
        // form has no `H` segment and its identity does not rotate. The exact
        // byte-stability tests below are the full no-rotation guarantee.
        let form = canonical_form(&td("fields:\n  a: String\n", "/v/note.type.yaml"));
        assert!(!form.contains('H'));
    }

    #[test]
    fn name_is_excluded_same_content_different_file_hashes_equal() {
        // Identity is (name, hash); the name is not in the hash.
        assert_eq!(
            hash("fields:\n  a: String\n", "/v/note.type.yaml"),
            hash("fields:\n  a: String\n", "/v/topic.type.yaml"),
        );
    }

    #[test]
    fn formatting_comments_and_key_order_are_excluded() {
        let a = "fields:\n  a: String\nsealed:\n  - note.x\n";
        let b = "# a comment\nsealed:\n  - note.x\nfields:\n  a: String   \n";
        assert_eq!(hash(a, "/v/note.type.yaml"), hash(b, "/v/note.type.yaml"));
    }

    #[test]
    fn docstrings_are_excluded_from_identity() {
        // `#:` docstrings are advisory, not semantic, so a copy that adds or
        // changes them stays in sync with its docless peer, not drifted.
        let plain = "fields:\n  a: String\n";
        let documented = "#: a documented type\nfields:\n  a: String   #: the a field\n";
        assert_eq!(
            hash(plain, "/v/note.type.yaml"),
            hash(documented, "/v/note.type.yaml"),
        );
    }

    #[test]
    fn docstring_wikilink_change_does_not_change_identity() {
        // A docstring's `[[...]]` is derived into a navigational edge, but the
        // docstring text stays out of the canonical form, so retargeting the
        // link leaves the type's identity untouched: the edge moves, the
        // identity does not.
        let links_a = "fields:\n  x: String   #: see [[alpha]]\n";
        let links_b = "fields:\n  x: String   #: see [[beta]]\n";
        assert_eq!(
            hash(links_a, "/v/t.type.yaml"),
            hash(links_b, "/v/t.type.yaml"),
        );
    }

    #[test]
    fn field_declaration_order_is_normalized() {
        let a = "fields:\n  a: String\n  b: Number\n";
        let b = "fields:\n  b: Number\n  a: String\n";
        assert_eq!(hash(a, "/v/note.type.yaml"), hash(b, "/v/note.type.yaml"));
    }

    #[test]
    fn quoting_a_field_shape_is_normalized_not_drift() {
        // A quoted scalar shape is the same value as the bare one, so the hash
        // must not see the quotes. A copy that quotes its shapes is in sync with
        // its bare-shape peer, not drifted.
        let bare = "fields:\n  description: String\n  topics?: topic*[]\n";
        let quoted = "fields:\n  description: \"String\"\n  topics?: \"topic*[]\"\n";
        assert_eq!(
            hash(bare, "/v/note.type.yaml"),
            hash(quoted, "/v/note.type.yaml"),
        );
    }

    #[test]
    fn enum_value_order_is_significant() {
        let a = "fields:\n  level: [low, high]\n";
        let b = "fields:\n  level: [high, low]\n";
        assert_ne!(hash(a, "/v/note.type.yaml"), hash(b, "/v/note.type.yaml"));
    }

    #[test]
    fn abstract_marker_is_significant() {
        // `abstract` narrows the valid-instance-set, so two same-name defs
        // differing only in it are distinct identities.
        let concrete = "fields:\n  a: String\n";
        let abstract_ = "abstract: true\nfields:\n  a: String\n";
        assert_ne!(
            hash(concrete, "/v/note.type.yaml"),
            hash(abstract_, "/v/note.type.yaml"),
        );
    }

    #[test]
    fn concrete_form_is_byte_stable_abstract_appends_only() {
        // Append-only-when-declared: an abstract type's canonical form is the
        // concrete form with a single trailing `A`, so a concrete type's bytes
        // are unchanged and no existing identity rotates. `abstract: false`
        // hashes equal to absent.
        let concrete = canonical_form(&td("fields:\n  a: String\n", "/v/note.type.yaml"));
        let explicit_false = canonical_form(&td(
            "abstract: false\nfields:\n  a: String\n",
            "/v/note.type.yaml",
        ));
        let abstract_true = canonical_form(&td(
            "abstract: true\nfields:\n  a: String\n",
            "/v/note.type.yaml",
        ));
        assert_eq!(concrete, explicit_false);
        assert!(!concrete.ends_with('A'));
        assert_eq!(abstract_true, format!("{concrete}A"));
    }

    #[test]
    fn required_meta_is_significant_and_appends_only() {
        // The `required:` obligation is contract-shaped, so it enters identity,
        // and it appends only when present, so a plain type stays byte-stable.
        let plain = canonical_form(&td("fields:\n  a: String\n", "/v/note.type.yaml"));
        let obligated = canonical_form(&td(
            "fields:\n  a: String\nmeta:\n  - required: p-meta\n",
            "/v/note.type.yaml",
        ));
        assert_ne!(plain, obligated);
        assert_eq!(obligated, format!("{plain}R1;6:p-meta"));
    }

    #[test]
    fn descriptive_meta_stays_excluded_beside_a_required_obligation() {
        // A descriptive value block does not change identity; only `required:` does.
        let a = canonical_form(&td(
            "fields:\n  a: String\nmeta:\n  - required: p-meta\n",
            "/v/note.type.yaml",
        ));
        let b = canonical_form(&td(
            "fields:\n  a: String\nmeta:\n  - required: p-meta\n  - type: display\n    label: hi\n",
            "/v/note.type.yaml",
        ));
        assert_eq!(a, b);
    }

    #[test]
    fn meta_is_excluded() {
        let bare = "fields:\n  a: String\n";
        let with_meta = "fields:\n  a: String\nmeta:\n  - type: display\n    label: hi\n";
        assert_eq!(
            hash(bare, "/v/note.type.yaml"),
            hash(with_meta, "/v/note.type.yaml")
        );
    }

    #[test]
    fn parent_claim_is_significant() {
        let a = "extends: alpha\nfields:\n  a: String\n";
        let b = "extends: beta\nfields:\n  a: String\n";
        assert_ne!(hash(a, "/v/note.type.yaml"), hash(b, "/v/note.type.yaml"));
    }

    #[test]
    fn parent_repo_qualifier_is_significant() {
        // Same base parent name, different `::repo` → distinct types. The
        // qualifier must enter the canonical form, mirroring the `use:` arm.
        let a = "extends: note::base\nfields:\n  a: String\n";
        let b = "extends: note::other\nfields:\n  a: String\n";
        assert_ne!(hash(a, "/v/note.type.yaml"), hash(b, "/v/note.type.yaml"));
    }

    #[test]
    fn body_section_is_significant() {
        let a = "body:\n  - section: Alpha\n";
        let b = "body:\n  - section: Beta\n";
        assert_ne!(hash(a, "/v/note.type.yaml"), hash(b, "/v/note.type.yaml"));
    }

    #[test]
    fn body_guidance_is_significant() {
        // Guidance is part of the body, the type's authoring surface, so it
        // is part of identity (unlike type-level meta).
        let a = "body:\n  - section: Alpha\n    guidance: do this\n";
        let b = "body:\n  - section: Alpha\n    guidance: do that\n";
        assert_ne!(hash(a, "/v/note.type.yaml"), hash(b, "/v/note.type.yaml"));
    }

    #[test]
    fn empty_body_and_absent_body_hash_equal() {
        let absent = "fields:\n  a: String\n";
        let empty = "fields:\n  a: String\nbody: []\n";
        assert_eq!(
            hash(absent, "/v/note.type.yaml"),
            hash(empty, "/v/note.type.yaml")
        );
    }
}
