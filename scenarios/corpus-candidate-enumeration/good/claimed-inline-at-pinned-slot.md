---
type: rationale
description: "Cost savings"
evidence:
  - type: evidence-item
    kind: paper
    source: "[[paper-b]]"
---

The sequence element claims `evidence-item` explicitly, so the candidate scan
runs at `/evidence/0` and `evidence-with-source` surfaces. A claim-less
element would be skipped — its identity is the slot's pin, and a candidate
there would advertise a claim that replaces the pin. Top-level cross-overlap
on `description` also surfaces `note` and `summary`.
