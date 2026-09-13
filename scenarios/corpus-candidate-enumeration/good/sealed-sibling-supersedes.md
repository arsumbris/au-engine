---
type: decision.pending
summary: "Awaiting input from stakeholder"
---

`decision.decided` surfaces as a candidate (same required field, `summary`),
but with `supersedes: [decision.pending]` — promoting requires dropping the
current leaf first per the multi-leaf-in-sealed-family rule. Sealed
parent `decision` is filtered out entirely.
