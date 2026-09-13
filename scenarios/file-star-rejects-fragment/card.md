---
type: card
asset: "[[doc.md^section]]"
---

`asset` is `file*`, which references a whole file.
The `^section` fragment addresses a part of the file, so it is rejected.
Use `any*` to address a `^block-id` or a typed block.
