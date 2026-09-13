---
type: doc
title: "Body contribution syntax"
---

# Reference

An inline contribution is written as inline code. Shown here double-ticked
so it reads as literal text, not a real contribution: `` `[:assumptions] some value` ``.

A fenced example wraps a shorter fence in a longer one, so the inner
triple-backtick block is content, not a close:

````markdown
```yaml [:assumptions]
type: assumption
claim: "example only — absorbed by the outer fence"
```
````
