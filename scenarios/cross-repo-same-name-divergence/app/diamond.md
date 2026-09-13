---
type:
  - note
  - note::base
title: bare
---

app's own `note` (title: String) beside base's `note` (title: Number) are two
distinct identities of one name. The field is divergent, so a BARE `title` cannot
pick a shape — the collision surfaces instead of silently validating against one.
Qualifying every use (`title{note}` and `title{note::base}`) resolves it.
