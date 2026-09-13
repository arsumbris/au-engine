---
type: c
desc:
---

The body carries a qualified contribution to the auto-unified `desc`, but the
qualifier names `bogus`, which is not in the instance's closure. The body
surface now validates the qualifier exactly as a frontmatter key does, so this
fires `qualifier-not-in-closure` — where it was previously silently ignored.

`[:desc{bogus}] hello`
