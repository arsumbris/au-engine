---
# `decision` is in closure (closure_of(decision.decided) ⊇ {decision}),
# but neither decision nor its ancestors declare `audience`.
# → qualifier-does-not-declare-field.
# Bare description satisfies note's required slot; no other diagnostics.
type: decision.decided
description: "x"
audience{decision}: "y"
---
