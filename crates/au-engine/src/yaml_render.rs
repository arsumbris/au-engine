//! The bounded JSON-to-YAML value renderer for the nested-record mutations
//! ([[spec - nested record edits - patch a record and append to a sequence by byte-splice, comments preserved]]).
//!
//! A patch or an appended element arrives as a `serde_json::Value` on the wire;
//! `edit_record` / `append_record` render it to block YAML text at a target
//! indent, then splice it in. The engine owns the serialization so a consumer
//! never hand-writes YAML.
//!
//! The renderer is total over JSON: scalars, arrays (block sequences), and
//! objects (block mappings) all emit. The v1 BOUNDS live in the splice layer,
//! not here: a field replace is scalar-only, and a re-type renders inline. This
//! module only turns a value into faithful YAML text.

use serde_json::{Map, Value};

/// A mapping's entries with `type` first, the rest in stable order. Without
/// serde_json's `preserve_order` a `Value` object iterates alphabetically, which
/// would bury the `type` claim; a freshly-rendered node has no comments to
/// disturb, so normalizing the claim to the front is free and reads naturally.
fn ordered(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut entries: Vec<(&String, &Value)> = map.iter().collect();
    entries.sort_by_key(|(k, _)| usize::from(k.as_str() != "type"));
    entries
}

/// A single YAML scalar token for an inline value position (`key: <token>` or
/// `- <token>`). `Err` on a non-scalar, the caller decides what that means in
/// its context (a field replace rejects, a block context descends instead).
pub(crate) fn scalar_token(value: &Value) -> Result<String, String> {
    match value {
        Value::Null => Ok("null".to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => Ok(quote_if_needed(s)),
        Value::Array(_) | Value::Object(_) => Err("value is not a scalar".to_string()),
    }
}

/// The claim value for a `type` patch: a bare token for one name, an inline flow
/// list for several (`[a, b]`), the accepted non-preferred list form. Replaces
/// the claim's value span in place, so it stays one line. `Err` on a shape that
/// is not a name or a list of names.
pub(crate) fn claim_value(value: &Value) -> Result<String, String> {
    match value {
        Value::String(_) => scalar_token(value),
        Value::Array(items) => {
            if items.is_empty() {
                return Err("a type claim must name at least one type".to_string());
            }
            let mut tokens = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(_) => tokens.push(scalar_token(item)?),
                    _ => return Err("a type claim list must be strings".to_string()),
                }
            }
            Ok(format!("[{}]", tokens.join(", ")))
        }
        _ => Err("a type claim must be a name or a list of names".to_string()),
    }
}

/// Render a new field, `key: value`, as one or more lines each prefixed with
/// `indent` spaces, ending in a newline. A scalar sits inline; a list or mapping
/// opens under the key and indents its body by two.
pub(crate) fn field(key: &str, value: &Value, indent: usize) -> String {
    let pad = " ".repeat(indent);
    match value {
        Value::Array(items) if !items.is_empty() => {
            let mut out = format!("{pad}{key}:\n");
            for item in items {
                out.push_str(&element(item, indent + 2));
            }
            out
        }
        Value::Object(map) if !map.is_empty() => {
            let mut out = format!("{pad}{key}:\n");
            for (k, v) in ordered(map) {
                out.push_str(&field(k, v, indent + 2));
            }
            out
        }
        // Empty collections and scalars sit inline.
        Value::Array(_) => format!("{pad}{key}: []\n"),
        Value::Object(_) => format!("{pad}{key}: {{}}\n"),
        scalar => format!(
            "{pad}{key}: {}\n",
            scalar_token(scalar).expect("a non-collection is a scalar")
        ),
    }
}

/// Render one block-sequence element, `- value`, as one or more lines each
/// prefixed with `indent` spaces, ending in a newline. A record element carries
/// its first key on the dash line, the rest aligned two under it.
pub(crate) fn element(value: &Value, indent: usize) -> String {
    let pad = " ".repeat(indent);
    match value {
        Value::Object(map) if !map.is_empty() => {
            let entries = ordered(map);
            let mut it = entries.into_iter();
            let (k0, v0) = it.next().expect("non-empty");
            // The first entry rides the dash line; its body, if a collection,
            // opens under the key at indent + 4 (past "- " plus one level).
            let mut out = match v0 {
                Value::Array(items) if !items.is_empty() => {
                    let mut s = format!("{pad}- {k0}:\n");
                    for item in items {
                        s.push_str(&element(item, indent + 4));
                    }
                    s
                }
                Value::Object(inner) if !inner.is_empty() => {
                    let mut s = format!("{pad}- {k0}:\n");
                    for (k, v) in ordered(inner) {
                        s.push_str(&field(k, v, indent + 4));
                    }
                    s
                }
                Value::Array(_) => format!("{pad}- {k0}: []\n"),
                Value::Object(_) => format!("{pad}- {k0}: {{}}\n"),
                scalar => format!(
                    "{pad}- {k0}: {}\n",
                    scalar_token(scalar).expect("a non-collection is a scalar")
                ),
            };
            // The remaining keys align two past the dash, under the first key.
            for (k, v) in it {
                out.push_str(&field(k, v, indent + 2));
            }
            out
        }
        Value::Array(items) if !items.is_empty() => {
            let mut out = format!("{pad}-\n");
            for item in items {
                out.push_str(&element(item, indent + 2));
            }
            out
        }
        Value::Object(_) => format!("{pad}- {{}}\n"),
        Value::Array(_) => format!("{pad}- []\n"),
        scalar => format!(
            "{pad}- {}\n",
            scalar_token(scalar).expect("a non-collection is a scalar")
        ),
    }
}

/// A double-quoted YAML string, or the plain form when it is unambiguous. The
/// engine reads standard YAML, so over-quoting is always safe; the plain form
/// is only taken when it cannot be misread (a keyword, a number, an indicator
/// start, a `[[wikilink]]`, an interior `: ` or ` #`).
fn quote_if_needed(s: &str) -> String {
    if is_plain_safe(s) {
        s.to_string()
    } else {
        double_quote(s)
    }
}

fn is_plain_safe(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~"
    ) {
        return false;
    }
    // A value YAML would read as a number must be quoted to stay a string.
    if s.parse::<f64>().is_ok() {
        return false;
    }
    let first = s.chars().next().expect("non-empty");
    // Indicator characters that cannot open a plain scalar, plus a leading dash
    // or space. Conservative: a plain-safe miss only over-quotes, never breaks.
    const UNSAFE_FIRST: &str = "!&*[]{}#|>@`\"'%,?:- \t";
    if UNSAFE_FIRST.contains(first) {
        return false;
    }
    if s.ends_with([' ', '\t', ':']) {
        return false;
    }
    // Interior sequences that end a plain scalar or start a comment.
    if s.contains(": ") || s.contains(" #") || s.contains('\n') || s.contains('\t') {
        return false;
    }
    true
}

fn double_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalar_tokens_cover_the_json_scalars() {
        assert_eq!(scalar_token(&json!(true)).unwrap(), "true");
        assert_eq!(scalar_token(&json!(42)).unwrap(), "42");
        assert_eq!(scalar_token(&json!("plain")).unwrap(), "plain");
        assert_eq!(scalar_token(&json!(null)).unwrap(), "null");
        assert!(scalar_token(&json!([1, 2])).is_err());
        assert!(scalar_token(&json!({"a": 1})).is_err());
    }

    #[test]
    fn strings_are_quoted_only_when_ambiguous() {
        // Plain-safe words stay plain.
        assert_eq!(scalar_token(&json!("action.done")).unwrap(), "action.done");
        assert_eq!(scalar_token(&json!("finished it")).unwrap(), "finished it");
        // A wikilink starts with '[', a keyword, a number, a leading colon: quote.
        assert_eq!(
            scalar_token(&json!("[[example-task.md]]")).unwrap(),
            "\"[[example-task.md]]\""
        );
        assert_eq!(scalar_token(&json!("true")).unwrap(), "\"true\"");
        assert_eq!(scalar_token(&json!("42")).unwrap(), "\"42\"");
        assert_eq!(scalar_token(&json!("a: b")).unwrap(), "\"a: b\"");
        assert_eq!(scalar_token(&json!("")).unwrap(), "\"\"");
    }

    #[test]
    fn claim_renders_bare_or_inline_list() {
        assert_eq!(claim_value(&json!("action.done")).unwrap(), "action.done");
        assert_eq!(
            claim_value(&json!(["action.done", "priority"])).unwrap(),
            "[action.done, priority]"
        );
        assert!(claim_value(&json!([])).is_err());
        assert!(claim_value(&json!(42)).is_err());
    }

    #[test]
    fn field_renders_a_scalar_inline() {
        assert_eq!(field("done", &json!(true), 8), "        done: true\n");
    }

    #[test]
    fn field_renders_a_string_list_as_a_block() {
        let out = field("outputs", &json!(["finished it", "noted the edge"]), 8);
        assert_eq!(
            out,
            "        outputs:\n          - finished it\n          - noted the edge\n"
        );
    }

    #[test]
    fn field_renders_a_wikilink_list_quoted() {
        let out = field("produced", &json!(["[[example-task.md]]"]), 8);
        assert_eq!(
            out,
            "        produced:\n          - \"[[example-task.md]]\"\n"
        );
    }

    #[test]
    fn field_renders_an_empty_list_inline() {
        assert_eq!(field("progressLog", &json!([]), 0), "progressLog: []\n");
    }

    #[test]
    fn element_renders_a_record_with_first_key_on_the_dash() {
        // The au-workflow action shape: `- type: action.open` then aligned fields.
        let out = element(
            &json!({"type": "action.open", "description": "first action"}),
            6,
        );
        assert_eq!(
            out,
            "      - type: action.open\n        description: first action\n"
        );
    }

    #[test]
    fn element_renders_a_scalar_item() {
        assert_eq!(element(&json!("note"), 2), "  - note\n");
    }

    #[test]
    fn element_renders_a_record_with_a_nested_list() {
        let out = element(&json!({"type": "phase", "actions": ["a"]}), 2);
        assert_eq!(out, "  - type: phase\n    actions:\n      - a\n");
    }
}
