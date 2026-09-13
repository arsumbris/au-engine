---
type:
  - decision.pending
  - decision.decided
summary: "A file claiming to be both pending AND decided"
---

# Multi-leaf claim

`decision` is sealed with two leaves (`decision.pending`, `decision.decided`). A sealed family is a discriminated union — one file can't simultaneously be both. The engine reports `multi-leaf-in-sealed-family` naming the family and the conflicting leaves.
