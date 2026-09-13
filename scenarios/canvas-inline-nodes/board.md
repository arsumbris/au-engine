---
type: canvas
nodes:
  - ^: root
    content: root
    position:
      x: 0
      y: 0
  - ^: child
    content: a child node
    position:
      x: 120
      y: 40
edges:
  - from: "[[^^root]]"
    to: "[[^^child]]"
    kind: tree
---

A small canvas.
The nodes are claim-less inline records, pinned to `node` by the slot.
The edges reference them by file-local block-id.
