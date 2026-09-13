---
type: note
description: "Prose with resolvable and dangling navigational links"
---

# Links

These resolve, no diagnostics:
[[target]] and [[target^known]] and [[^own-marker]] and [[#links]].

These dangle, each a warning:
[[no-such-file]] misses its file.
[[target^missing-id]] misses the id in the target.
[[^missing-local]] misses the id in this file.
[[target#No Such Heading]] misses the heading.

A local marker for the resolvable case.
^own-marker
