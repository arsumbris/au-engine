---
type:
  - decision.decided
  - source.url
summary: "shipped"
title: "spec"
external_url: "https://example.com/spec"
---

# Mixin across disjoint sealed families

One leaf from `decision` (sealed) and one from `source` (sealed). The multi-leaf rule is per-family — these families don't overlap, so the mixin is fine.
