---
type: captured
description: "A captured trace event"
data:
---

# Data

```yaml [:data]
type: point
note: opaque, no x or y here
```

The block above claims `point` but omits its required `x` / `y`.
Because `data` is `opaque`, the block is uninterpreted and not validated.
