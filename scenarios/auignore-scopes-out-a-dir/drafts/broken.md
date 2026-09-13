---
type: nonexistent-type
---

This claims a type that does not exist. It WOULD raise a diagnostic, but
`drafts/` is scoped out by `.arsumbris/.auignore`, so it never enters the
graph and the repo validates clean.
