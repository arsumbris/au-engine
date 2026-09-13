---
type: decision
description: "Migrate dev DB to SQLite"
status:
rationale:
causes:
sources:
---

# Why

The migration was driven by `[:causes] shared-volume contention`.

## Decision

The decision was finalized on `[:status] made`.

```yaml [:rationale]
type: rationale
description: "SQLite removes the shared-volume bottleneck at dev volume"
```

# Sources

See [[paper-a:sources]] and [[paper-b:sources]] for the benchmarks
that supported the decision.
