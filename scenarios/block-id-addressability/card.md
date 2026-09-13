---
type: card
title: "[[notes^obs]]"
count: "[[notes^obs]]"
assumption: "[[notes^obs]]"
source: "[[notes^missing]]"
---

Four slots, one wikilink shape:
- `title` is a String slot, the value is just that string, nothing resolves.
- `count` is a Number slot, a plain shape mismatch, no block-id diagnostic.
- `assumption` admits a reference; `^obs` is a plain block, so the FILE is the referent — `notes` is itself an `assumption`, so it satisfies the slot, and `^obs` is a navigational anchor that resolves.
- `source` admits a reference; `^missing` is absent, so the FILE is still the referent (a valid assumption) and the dangling anchor is a navigational warning, not an error.

Navigational prose like [[notes^obs]] contributes nothing.
