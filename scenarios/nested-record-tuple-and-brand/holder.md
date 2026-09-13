---
type: outer
box:
  type: inner
  pt: point(1, 2)
  region: (0, 0, 10, 10)
  len: meter(5)
---

# Holder

A nested inline record whose fields are a tuple brand, an inline tuple, and a
scalar brand — each must read RESOLVED, not as a raw string.
