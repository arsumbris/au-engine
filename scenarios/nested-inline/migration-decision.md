---
type: decision-with-rationale
summary: "Migrate to SQLite"
rationale:
  description: "Performance gains over the existing layer."
  evidence:
    source: "Benchmark report 2026-Q1"
---

# A decision, two levels of inline values deep

`rationale` is the slot at level 1.
`rationale.evidence` is `evidence-item` at level 2.
Each level is a YAML map mirroring the type-def's frontmatter — extracts cleanly to its own file by replacing the map with a `[[wikilink]]` later.
