---
type: evidence
support: "[[some-thesis]]"
---

# Evidence with the wrong target

`support` is declared `rationale*` — the target's `type:` closure must include `rationale`. But `[[some-thesis]]` is a thesis, not a rationale. Fires `reference-target-type-mismatch`.
