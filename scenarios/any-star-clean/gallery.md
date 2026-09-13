---
type: gallery
items:
  - "[[diagram.pdf]]"
  - "[[my-note]]"
---

`items` is `any*[]`, so each entry references any node by existence.
The untyped PDF asset and the typed `note` both resolve, with no closure check.
A `note*[]` slot would reject the PDF; `any*` does not.
