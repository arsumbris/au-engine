---
type: sealed-host
choice:
  summary: "I forgot the inline type: claim"
---

# Sealed slot, missing inline `type:`

`choice: decision` is a sealed-parent record slot. The inline value MUST declare `type:` and the leaf must be non-sealed (e.g. `decision.pending` or `decision.decided`). Omitting `type:` here fires `inline-value-missing-type`.
