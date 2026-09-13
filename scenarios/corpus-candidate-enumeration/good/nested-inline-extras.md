---
type: rationale
description: "Performance gains"
evidence:
  - type: evidence-item
    kind: paper
    source: "[[paper-a]]"
  - type: evidence-item
    kind: observation
---

Each inline `evidence-item` carries an explicit `type:`. The first element
also provides `source` — `evidence-with-source` surfaces as a candidate at
`/evidence/0` (nested scope). Top-level `note` and `summary` also
surface (both require `description`, present here) — the cross-overlap
that the amendment now exposes.
