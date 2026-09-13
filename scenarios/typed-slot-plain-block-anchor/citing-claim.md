---
type: claim
evidence:
  - "[[rival-post^passage]]"
  - "[[rival-post^gone]]"
---

# A claim citing an exact passage

`[[rival-post^passage]]` cites a plain prose passage. `passage` is not a typed block, so the FILE is the referent — `rival-post` is a `post`, which satisfies `post*` — and `^passage` is a navigational anchor that resolves. No `block-id-not-typed`.

`[[rival-post^gone]]` names an absent block-id. The file is still the referent (a valid `post`), and the dangling anchor is a navigational warning, not an error.
