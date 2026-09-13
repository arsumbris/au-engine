---
type: card
nav: "[[notes^obs]]"
val: "[[notes^^obs]]"
---

Two slots, the same plain block `^obs`, two sigils.

`nav` uses a bare `^obs`: navigational, so the FILE `notes` is the referent.
`notes` is an `assumption`, which satisfies `assumption*`, and `^obs` is a jump
anchor that resolves. Clean.

`val` uses `^^obs`: a block-referent, so the block is the demanded value. `obs`
is a plain prose block with no typed yaml fence, so it carries no type —
`block-id-not-typed`. The one error the scenario asserts.
