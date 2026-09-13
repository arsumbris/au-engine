# au-grammar

Slot-expression mini-language → `Shape` AST. Parses the right-hand side of field declarations: primitives (`String`, `Number`, `Boolean`, `Date`, `DateTime`, `Url`), inline enums (`[v1, v2]`), bare type-def names, suffixes (`*`, `&`, `[]`, `[+]`), and compound expressions (`<X | Y>`, `<X & Y>`).

Independent of the type-graph — produces a `Shape` from a string. Resolution against actual type-defs happens in `au-core`.

Currently recognizes primitives, inline closed enums (elements follow the type-name regex), bare-name record slots (`rationale`), typed references (`name*`, including the built-in `file*`), inline-or-reference (`name&`), the list suffix `[]` and its atomic non-empty variant `[+]`, compound expressions (`<X | Y>`, `<X & Y>`), and the suffix matrix on compounds (`<...>*`, `<...>&`, `<...>[]`, `<...>[+]`). `Shape::List { inner, non_empty }` carries the cardinality flag; `Shape::Union` / `Shape::Intersection` carry the value-shape compound; `Shape::CompoundReference { mode, op, branches }` carries the suffix-bearing form (branches accept all three name-bearing variants — Reference, Record, InlineOrReference — and reduce to a flat `Vec<String>`; primitives and enums in the compound are rejected at parse time). The spec-canonical bare-name compound form `<rationale | thesis>*` parses to the same AST as the explicit-`*`-per-branch form `<rationale* | thesis*>*`.

Hard-rejected as `shape-syntax-error`: `*` / `&` on primitives, inline enums, or compounds containing them; bare `file` and `file&`; mixed `|`/`&` operators in one compound; empty / single-branch / leading-or-trailing-separator compounds. Names or enum elements violating the type-name regex are also `shape-syntax-error`.
