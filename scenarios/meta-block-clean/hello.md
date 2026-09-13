---
type: note
description: "An instance of note. The display-meta block is type-level metadata on note itself — it doesn't flow into instance frontmatter."
---

# Hello

The `note` type-def carries a `display-meta` block. Meta is type-level metadata: it lives on the type-def, not on the instance. Validation checks the sub-region body against `display-meta`'s contract.
