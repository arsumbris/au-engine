---
type: capture
detail:
---

```[:detail]
type: production
replicas: 3
```

Meant as text, but it is a well-formed typed mapping, so the union takes the
record branch. `production` is not a type, so the claim fails loudly.
