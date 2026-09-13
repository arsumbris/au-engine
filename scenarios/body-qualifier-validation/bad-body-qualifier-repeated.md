---
type: c
desc:
---

The same bogus body qualifier appears at two sites. Each is its own fix-site, so
each fires `qualifier-not-in-closure` at its own span — every offending location
is surfaced at once, rather than one-at-a-time across re-validations.

`[:desc{bogus}] hello`

`[:desc{bogus}] hello`
