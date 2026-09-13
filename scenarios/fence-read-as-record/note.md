---
type: capture
detail:
---

# Detail

```[:detail]
type: point
x: over there
y: 4
```

The author meant literal text, but the content is a well-formed mapping
carrying `type:`, so the union's record branch wins and `x` fails `Number`.
The hint names that cause; the shape error alone would not.
