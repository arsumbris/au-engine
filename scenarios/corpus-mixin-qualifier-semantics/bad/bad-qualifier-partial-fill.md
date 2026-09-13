---
# partial-fill case. Both journal-entry and headlined declare
# required `title: String` (token-equal, two distinct originators).
# Providing only `title{journal-entry}` satisfies (journal-entry, title)
# but not (headlined, title) → required-field-absent fires for the
# headlined origin specifically.
type:
  - journal-entry
  - headlined
title{journal-entry}: "x"
---
