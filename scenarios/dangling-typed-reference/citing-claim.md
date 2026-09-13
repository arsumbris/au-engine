---
type: claim
supports: "[[future-claim]]"
---

# A claim that cites a not-yet-written claim

`supports` is declared `claim*`, a typed reference. Its target `[[future-claim]]` does not exist in the repo yet.

A dangling typed reference is open-world growth, the target may be authored next, so it fires `reference-target-missing` at `warning`, not `error`. The dangling link doubles as a forge signal, the claim the graph wants next.
