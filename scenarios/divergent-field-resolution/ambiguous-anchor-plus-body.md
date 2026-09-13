---
# A divergent field carried by the body needs a qualified frontmatter anchor
# (`title{c}:`) AND a body marker (`[:title{c}]`). The ambiguous qualifier `c`
# is written at both sites, and each fires `qualifier-ambiguous` at its own span:
# these diagnostics anchor at the offending token, so every fix-site is surfaced
# at once. Filling no origin, both required origins also fire.
type: c
title{c}:
---

`[:title{c}] hello`
