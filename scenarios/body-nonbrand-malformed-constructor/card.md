---
type: holder
ref:
---

A constructor-shaped but malformed value at a non-brand reference slot,
contributed from the body: `[:ref] bad(x`.

Since `note*` is a plain reference (not a brand), this reads as an invalid
reference value, the same verdict the frontmatter surface gives — never a
`malformed-constructor` (that fires only at a brand slot).
