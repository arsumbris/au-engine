---
# The resolved counterpart to diamond.md: qualifying each identity of `note`
# (`title{note}` = app's String, `title{note::base}` = base's Number) fills both
# required origins, so the divergent field resolves CLEAN.
type:
  - note
  - note::base
title{note}: "a string"
title{note::base}: 42
---
