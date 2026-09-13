---
type: rating
level: high
---

# Use the broken type

`rating`'s `level` slot has an invalid enum element (`1abc` starts with
a digit). The shape-parse error surfaces at this use site —
shape-syntax-error fires because the slot's parsed_shape is an Err
that validation now needs to consult.
